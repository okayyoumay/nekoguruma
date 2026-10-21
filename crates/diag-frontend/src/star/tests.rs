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
    let source = r#""""A docstring."""
LIMIT = 3
KEYS = {"a": 1}

def helper(x, y):
    """Doc."""
    return x + y

# @section uninterruptible expected=120000
def write():
    # @idempotency checkState
    diag.flash_transfer(1)

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
    write()
    v = KEYS["a"] if total else -1
"#;
    let checked = passes(source);
    assert_eq!(
        checked.annotations,
        [
            Annotation::Section {
                line: 9,
                def: 10,
                kind: SectionKind::Uninterruptible,
                expected_millis: Some(120_000)
            },
            Annotation::Idempotency {
                line: 11,
                statement_at: source.find("diag.flash_transfer").unwrap() as u32,
                value: Idempotency::CheckState
            },
            Annotation::MaxIterations {
                line: 23,
                for_line: 24,
                limit: 10
            },
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
    # @idempotency maybe
    # @frobnicate
    # @endsection
    x = \"# @not an annotation\"

# @section uninteruptible
# @section interruptible expected=5 extra=1
def other():
    pass
";
    let found = codes(source);
    assert!(
        found
            .iter()
            .all(|(code, _)| *code == StarCode::BadAnnotation),
        "{found:?}"
    );
    let lines: Vec<u32> = found.iter().map(|(_, line)| *line).collect();
    assert_eq!(lines, [2, 5, 7, 8, 9, 12, 13]);
}

/// A section is a whole top-level `def`: the annotation must come directly before one, outside
/// every function body (4.5, ADR-301).
#[test]
fn a_section_marks_a_top_level_def() {
    let source = "\
def helper():
    # @section uninterruptible
    pass

def main():
    if True:
        # @section interruptible
        pass
    x = 1
# @section interruptible
    helper()

# @section interruptible
X = 1
";
    assert_eq!(
        codes(source),
        [
            (StarCode::BadAnnotation, 2),
            (StarCode::BadAnnotation, 7),
            (StarCode::BadAnnotation, 10),
            (StarCode::BadAnnotation, 13),
        ]
    );
}

/// A comment after the last statement of a body belongs to that body, even when a top-level
/// `def` follows it.
#[test]
fn a_section_comment_inside_the_previous_body_is_rejected() {
    let source = "\
def helper():
    pass
    # @section interruptible
def main():
    helper()
";
    assert_eq!(codes(source), [(StarCode::BadAnnotation, 3)]);
}

/// A comment inside a multi-line constant is not directly before the `def` after it.
#[test]
fn a_section_comment_inside_a_constant_is_rejected() {
    let source = "\
X = {
    # @section interruptible
    \"a\": 1,
}
def main():
    pass
";
    assert_eq!(codes(source), [(StarCode::BadAnnotation, 2)]);
}

/// A `@section` written after a one-line `def` or inside the last call of the previous body is
/// not directly before the next `def`.
#[test]
fn a_section_comment_in_the_previous_def_is_rejected() {
    assert_eq!(
        codes("def helper(): pass  # @section uninterruptible\ndef main():\n    helper()\n"),
        [(StarCode::BadAnnotation, 1)]
    );
    let source = "\
def helper():
    diag.log(1,
# @section uninterruptible
        \"x\")
def main():
    helper()
";
    assert_eq!(codes(source), [(StarCode::BadAnnotation, 3)]);
    // Comments and blank lines between functions are fine.
    passes(
        "def helper():\n    pass\n\n# Writes.\n# @section uninterruptible\n\n# More.\ndef main():\n    helper()\n",
    );
}

/// `@preconditions` stands directly before `def main()` and names the five preconditions (4.9).
#[test]
fn preconditions_stand_directly_before_main() {
    passes("# @preconditions voltage\ndef main():\n    pass\n");
    passes(
        "# @preconditions voltage externalSupply ignition engine vehicleSpeed\n\
         def main():\n    pass\n",
    );
    // Other comments and blank lines may come between, and a section may share the `def`.
    passes("def f():\n    pass\n\n# @preconditions ignition\n\n# More.\ndef main():\n    f()\n");
    passes("# @section interruptible\n# @preconditions ignition\ndef main():\n    pass\n");
    let bad = |source: &str, lines: &[u32]| {
        let found = codes(source);
        assert!(
            found
                .iter()
                .all(|(code, _)| *code == StarCode::BadAnnotation),
            "{source}: {found:?}"
        );
        let found: Vec<u32> = found.iter().map(|(_, line)| *line).collect();
        assert_eq!(found, lines, "{source}");
    };
    // Not before `main`: another `def`, a constant, inside a body, after code.
    bad(
        "# @preconditions voltage\ndef f():\n    pass\n\ndef main():\n    f()\n",
        &[1],
    );
    bad(
        "# @preconditions voltage\nX = 1\ndef main():\n    pass\n",
        &[1],
    );
    bad(
        "def main():\n    # @preconditions voltage\n    pass\n",
        &[2],
    );
    bad("def main():\n    pass\n# @preconditions voltage\n", &[3]);
    bad(
        "X = 1  # @preconditions voltage\ndef main():\n    pass\n",
        &[1],
    );
    // A name that is not a precondition (the table's spelling is not the annotation's), one
    // named twice, none, and a second annotation.
    bad(
        "# @preconditions external_supply\ndef main():\n    pass\n",
        &[1],
    );
    bad(
        "# @preconditions voltage voltage\ndef main():\n    pass\n",
        &[1],
    );
    bad("# @preconditions\ndef main():\n    pass\n", &[1]);
    bad(
        "# @preconditions voltage\n# @preconditions engine\ndef main():\n    pass\n",
        &[2],
    );
}

#[test]
fn a_def_takes_one_section() {
    let source = "\
# @section interruptible
# @section uninterruptible
def main():
    pass
";
    assert_eq!(codes(source), [(StarCode::BadAnnotation, 2)]);
}

#[test]
fn the_end_of_a_section_is_the_end_of_its_def() {
    let source = "\
# @section interruptible
def main():
    diag.wait(1)
    # @endsection
";
    assert_eq!(codes(source), [(StarCode::BadAnnotation, 4)]);
}

/// A section function called from inside another section, directly or through a helper, is
/// rejected at the call; calling one from outside every section is not (ADR-301).
#[test]
fn sections_do_not_nest() {
    let source = "\
# @section interruptible
def inner():
    diag.wait(1)

def helper():
    inner()

# @section uninterruptible expected=10
def outer():
    inner()
    helper()

# @section recoveryRequired
def last():
    diag.wait(2)

def main():
    inner()
    outer()
    helper()
    last()
";
    assert_eq!(
        codes(source),
        [(StarCode::BadAnnotation, 6), (StarCode::BadAnnotation, 10)]
    );
}

/// A section function in a recursive cycle is reported as recursion only.
#[test]
fn recursion_through_a_section_is_reported_once() {
    let source = "\
# @section interruptible
def a():
    b()

# @section interruptible
def b():
    a()

def main():
    a()
";
    let found = codes(source);
    assert!(
        found
            .iter()
            .all(|(code, _)| *code == StarCode::UnboundedNotAllowed),
        "{found:?}"
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
    // A comment at the indentation of `def` is outside the body: it precedes the next top-level
    // statement, here a constant rather than a `def`.
    assert_eq!(
        codes("def main():\n    pass\n# @section interruptible\n\nX = 1\n"),
        [(StarCode::BadAnnotation, 3)]
    );
    // A comment after the last statement of the body still belongs to it.
    assert_eq!(
        codes("def main():\n    diag.wait(1)\n    # @idempotency safe\n\nX = 1\n"),
        [(StarCode::BadAnnotation, 3)]
    );
}

#[test]
fn a_negative_annotation_value_is_out_of_range() {
    assert_eq!(
        codes(
            "def main():\n    # @maxIterations -1\n    for i in range(3):\n        pass\n    \
             pass\n\n# @section uninterruptible expected=-1\ndef write():\n    pass\n"
        ),
        [
            (StarCode::ArgumentOutOfRange, 2),
            (StarCode::ArgumentOutOfRange, 7),
        ]
    );
    assert_eq!(
        codes("def main():\n    # @maxIterations -x\n    for i in range(3):\n        pass\n"),
        [(StarCode::BadAnnotation, 2)]
    );
}

#[test]
fn deeply_nested_loops_are_checked_in_linear_passes() {
    // Each loop body binds `d` after using it, so every level needs a second pass to see it. The
    // loops nest as deep as the block bound allows (2.8).
    let depth = MAX_BLOCK_DEPTH - 1;
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

#[test]
fn a_default_value_is_checked_inside() {
    assert_eq!(
        codes(
            "def helper(x = fail(\"bad\"), y = print(None)):\n    pass\n\ndef main():\n    \
             f = lambda z = dir(1): z\n"
        ),
        [
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::FailNotAllowed, 1),
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::BuiltinNotAllowed, 1),
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::ClosureNotAllowed, 5),
            (StarCode::UnsupportedSyntax, 5),
            (StarCode::BuiltinNotAllowed, 5),
        ]
    );
}

#[test]
fn break_and_continue_carry_their_bindings() {
    // What follows a `break` is not reached, and the loop ends with the binding at the `break`.
    assert_eq!(
        codes(
            "def main():\n    d = 0\n    k = \"a\"\n    # @maxIterations 3\n    for i in \
             range(3):\n        d = {\"a\": 1}\n        break\n        d = 0\n    x = d[k]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 9)]
    );
    // A `continue` carries its binding to the next pass.
    assert_eq!(
        codes(
            "def main():\n    d = 0\n    k = \"a\"\n    # @maxIterations 3\n    for i in \
             range(3):\n        x = d[k]\n        d = {\"a\": 1}\n        if i:\n            \
             continue\n        d = 0\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 6)]
    );
    // A path that returns does not reach what follows.
    passes(
        "def helper(k):\n    d = {\"a\": 1}\n    if k:\n        return 1\n    else:\n        \
         d = [1]\n    return d[k]\n\ndef main():\n    helper(0)\n",
    );
}

#[test]
fn an_inner_loop_reaches_its_own_fixpoint_inside_an_outer_one() {
    // `a` becomes a dict on the inner loop's second pass; `b` is reset after the inner loop.
    let after = "def main():\n    a = [1]\n    b = [1]\n    k = 0\n    # @maxIterations 2\n    \
                 for i in range(2):\n        x = a[k]\n        # @maxIterations 2\n        for j in \
                 range(2):\n            a = b\n            b = {\"x\": 1}\n        b = [1]\n";
    assert_eq!(codes(after), [(StarCode::DynamicKeyNotAllowed, 7)]);
    // The same with `b` reset before the inner loop.
    let before = "def main():\n    a = [1]\n    b = [1]\n    k = 0\n    # @maxIterations 2\n    \
                  for i in range(2):\n        x = a[k]\n        b = [1]\n        # @maxIterations 2\n        \
                  for j in range(2):\n            a = b\n            b = {\"x\": 1}\n";
    assert_eq!(codes(before), [(StarCode::DynamicKeyNotAllowed, 7)]);
}

#[test]
fn a_function_or_constant_named_diag_hides_the_module() {
    assert_eq!(
        codes("def diag():\n    return 1\n\ndef main():\n    diag.request(0x22, {})\n"),
        [
            (StarCode::BuiltinNotAllowed, 5),
            (StarCode::FunctionValueNotAllowed, 5),
        ]
    );
    passes("diag = \"abc\"\nLABEL = diag.upper()\n\ndef main():\n    pass\n");
}

#[test]
fn and_and_or_give_one_of_their_operands() {
    assert_eq!(
        codes(
            "def main():\n    k = \"a\"\n    res = diag.request(0x22, {}) or {}\n    x = res[k]\n    \
             y = res[\"fields\"][k]\n    z = (k and {\"a\": 1})[k]\n"
        ),
        [
            (StarCode::DynamicKeyNotAllowed, 4),
            (StarCode::DynamicKeyNotAllowed, 5),
            (StarCode::DynamicKeyNotAllowed, 6),
        ]
    );
}

#[test]
fn loops_that_reset_each_others_bindings_stay_cheap() {
    // Each level resets the next level's name before that loop and binds its own after it, so
    // every level sees different bindings; a cache by whole entry state grows exponentially.
    let depth = 20;
    let mut source = String::from("def main():\n    k = \"a\"\n");
    for level in 0..=depth {
        source.push_str(&format!("    v{level} = 0\n"));
    }
    let mut lines = 2 + depth as u32 + 1;
    for level in 0..depth {
        let indent = "    ".repeat(level + 1);
        source.push_str(&format!(
            "{indent}v{next} = 0\n{indent}# @maxIterations 2\n{indent}for i{level} in range(2):\n",
            next = level + 1
        ));
        lines += 3;
    }
    let indent = "    ".repeat(depth + 1);
    source.push_str(&format!("{indent}x = v0[k]\n"));
    let read = lines + 1;
    for level in (0..depth).rev() {
        let indent = "    ".repeat(level + 2);
        source.push_str(&format!("{indent}v{level} = {{\"a\": 1}}\n"));
    }
    assert_eq!(codes(&source), [(StarCode::DynamicKeyNotAllowed, read)]);
}

#[test]
fn a_loop_after_a_jump_in_its_outer_body_is_still_reached() {
    // The outer body ends with `continue`, which leaves the quiet walk unreached; the inner loop
    // must still be checked with every binding a pass can start from.
    assert_eq!(
        codes(
            "def main():\n    k = \"a\"\n    # @maxIterations 2\n    for i in range(2):\n        \
             d = 0\n        # @maxIterations 2\n        for j in range(2):\n            x = d[k]\n            \
             d = {\"a\": 1}\n        continue\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 8)]
    );
}

#[test]
fn a_statement_takes_one_idempotency() {
    assert_eq!(
        codes(
            "def main():\n    # @idempotency safe\n    # @idempotency unsafe\n    diag.wait(1)\n"
        ),
        [(StarCode::BadAnnotation, 3)]
    );
    // One each for two statements, also on one line, is fine.
    assert_eq!(
        codes(
            "def main():\n    # @idempotency safe\n    diag.wait(1)\n    # @idempotency unsafe\n    diag.wait(2)\n"
        ),
        []
    );
}

#[test]
fn an_idempotency_stands_on_a_line_of_its_own() {
    assert_eq!(
        codes("def main():\n    diag.wait(1)  # @idempotency unsafe\n    diag.wait(2)\n"),
        [(StarCode::BadAnnotation, 2)]
    );
    assert_eq!(
        codes("def main():\n    # @idempotency unsafe\n    diag.wait(1)\n"),
        []
    );
}

#[test]
fn only_one_comment_marker_starts_an_annotation() {
    let source = "def main():\n    ## @idempotency safe\n    ## @unknown\n    # @idempotency \
                  checkState\n    diag.wait(1)\n";
    let checked = passes(source);
    assert_eq!(
        checked.annotations,
        [Annotation::Idempotency {
            line: 4,
            statement_at: source.find("diag.wait").unwrap() as u32,
            value: Idempotency::CheckState
        }]
    );
}

#[test]
fn a_top_level_name_is_bound_once() {
    assert_eq!(
        codes(
            "K = \"a\"\nTABLE = {\"a\": 1}\nITEM = TABLE[K]\nTABLE = [1]\n\ndef helper():\n    \
             return 1\n\nhelper = 3\n\ndef main():\n    helper()\n\nmain = 3\n"
        ),
        [
            (StarCode::UnsupportedSyntax, 4),
            (StarCode::UnsupportedSyntax, 9),
            (StarCode::UnsupportedSyntax, 14),
        ]
    );
    assert_eq!(
        codes("def f():\n    pass\n\ndef f():\n    pass\n\ndef main():\n    f()\n"),
        [(StarCode::UnsupportedSyntax, 4)]
    );
}

#[test]
fn a_known_dict_is_not_sliced() {
    assert_eq!(
        codes(
            "def main():\n    res = diag.request(0x22, {})\n    x = res[:1]\n    raw = res[\"raw\"]\n    \
             y = raw[1:2]\n"
        ),
        [(StarCode::DynamicKeyNotAllowed, 3)]
    );
}

#[test]
fn an_excluded_builtin_is_not_a_value() {
    assert_eq!(
        codes(
            "def helper(x):\n    return x\n\ndef main():\n    a = sorted\n    helper(zip)\n    \
             return set\n"
        ),
        [
            (StarCode::FunctionValueNotAllowed, 5),
            (StarCode::FunctionValueNotAllowed, 6),
            (StarCode::FunctionValueNotAllowed, 7),
        ]
    );
    // A local of that spelling is a value.
    passes("def helper(sorted):\n    return sorted\n\ndef main():\n    helper(1)\n");
}

#[test]
fn bytes_is_not_a_value() {
    assert_eq!(
        codes("def main():\n    a = bytes\n    return bytes\n"),
        [
            (StarCode::FunctionValueNotAllowed, 2),
            (StarCode::FunctionValueNotAllowed, 3),
        ]
    );
    passes("def helper(bytes):\n    return bytes\n\ndef main():\n    helper(1)\n");
}

#[test]
fn constants_on_one_line_are_constants() {
    passes("A = 1; B = 2\n\ndef main():\n    x = A + B\n");
}

#[test]
fn a_rejected_top_level_target_is_checked_inside() {
    assert_eq!(
        codes("D = {}\nD[fail(\"x\")] = 1\nD.x = print(1)\n\ndef main():\n    pass\n"),
        [
            (StarCode::UnsupportedSyntax, 2),
            (StarCode::DynamicKeyNotAllowed, 2),
            (StarCode::FailNotAllowed, 2),
            (StarCode::UnsupportedSyntax, 3),
            (StarCode::BuiltinNotAllowed, 3),
        ]
    );
}

#[test]
fn a_dict_literal_key_is_a_string_literal() {
    assert_eq!(
        codes("def main():\n    k = \"did\"\n    diag.request(0x22, {k: 0xF190, \"a\": 1})\n"),
        [(StarCode::DynamicKeyNotAllowed, 3)]
    );
}

#[test]
fn chr_and_ord_are_not_values() {
    assert_eq!(
        codes("def main():\n    x = chr\n    y = ord\n"),
        [
            (StarCode::FunctionValueNotAllowed, 2),
            (StarCode::FunctionValueNotAllowed, 3),
        ]
    );
}

#[test]
fn an_annotation_keyword_follows_the_at_sign() {
    assert_eq!(
        codes(
            "def main():\n    # @ section uninterruptible\n    diag.wait(1)\n    # @ endsection\n    \
             # @\n    pass\n"
        ),
        [
            (StarCode::BadAnnotation, 2),
            (StarCode::BadAnnotation, 4),
            (StarCode::BadAnnotation, 5),
        ]
    );
}

#[test]
fn many_calls_are_checked_for_recursion_once_per_function() {
    let calls = 20_000;
    let mut source = String::from("def c():\n    pass\n\ndef b():\n");
    source.push_str(&"    c()\n".repeat(calls));
    source.push_str("\ndef main():\n");
    source.push_str(&"    b()\n".repeat(calls));
    let started = std::time::Instant::now();
    passes(&source);
    // Quadratic in the calls this took seconds; linear it takes a fraction of one.
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn an_assigned_element_of_a_dict_has_a_literal_key() {
    assert_eq!(
        codes("def main():\n    d = {\"a\": 1}\n    k = \"a\"\n    d[k] = 2\n    d[k] += 2\n"),
        [
            (StarCode::UnsupportedSyntax, 4),
            (StarCode::DynamicKeyNotAllowed, 4),
            (StarCode::UnsupportedSyntax, 5),
            (StarCode::DynamicKeyNotAllowed, 5),
        ]
    );
}

#[test]
fn a_dict_indexed_with_two_keys_is_a_computed_key() {
    assert_eq!(
        codes("def main():\n    d = {\"a\": 1}\n    x = d[1, 2]\n"),
        [
            (StarCode::DynamicKeyNotAllowed, 3),
            (StarCode::UnsupportedSyntax, 3),
        ]
    );
}

#[test]
fn a_nested_def_is_a_function_value_and_can_recurse() {
    assert_eq!(
        codes("def main():\n    def g():\n        g()\n    x = g\n"),
        [
            (StarCode::ClosureNotAllowed, 2),
            (StarCode::UnboundedNotAllowed, 3),
            (StarCode::FunctionValueNotAllowed, 4),
        ]
    );
}

#[test]
fn a_typed_star_parameter_reports_the_type_too() {
    assert_eq!(
        codes("def f(*args: int):\n    pass\n\ndef main():\n    pass\n"),
        [
            (StarCode::UnsupportedSyntax, 1),
            (StarCode::UnsupportedSyntax, 1),
        ]
    );
}

#[test]
fn a_long_chain_of_functions_is_checked_in_linear_time() {
    // Each function calls the next; what each reaches, kept per function, would be quadratic.
    let functions = 20_000;
    let mut source = String::new();
    for at in 0..functions {
        source.push_str(&format!("def f{at}():\n    f{}()\n\n", at + 1));
    }
    source.push_str(&format!(
        "def f{functions}():\n    pass\n\ndef main():\n    f0()\n"
    ));
    let started = std::time::Instant::now();
    passes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    // Closing the chain into a cycle makes every call in it recursive.
    let cycle = source.replace(
        &format!("def f{functions}():\n    pass"),
        &format!("def f{functions}():\n    f0()"),
    );
    let errors = check("test.star", &cycle).unwrap_err();
    assert_eq!(errors.len(), functions + 1);
    assert!(
        errors
            .iter()
            .all(|e| e.code == StarCode::UnboundedNotAllowed)
    );
}

/// A `@section` written after a statement is on that statement's line, so it is not directly
/// before a `def`.
#[test]
fn an_inline_section_comment_is_judged_by_its_line() {
    assert_eq!(
        codes("X = 1  # @section interruptible\ndef main():\n    pass\n"),
        [(StarCode::BadAnnotation, 1)]
    );
    assert_eq!(
        codes("def main():\n    x = 1  # @section interruptible\n    diag.wait(1)\n"),
        [(StarCode::BadAnnotation, 2)]
    );
}

#[test]
fn many_annotations_are_looked_up_in_logarithmic_time() {
    let mut source = String::new();
    for at in 0..2_000 {
        source.push_str(&format!("def f{at}():\n"));
        source.push_str(&"    # @idempotency safe\n    diag.wait(1)\n".repeat(10));
        source.push('\n');
    }
    source.push_str("def main():\n    f0()\n");
    let started = std::time::Instant::now();
    let checked = passes(&source);
    assert_eq!(checked.annotations.len(), 20_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn a_snippet_of_a_long_line_is_bounded() {
    let entries = 20_000;
    let source = format!(
        "def main():\n    x = [{}]\n",
        vec!["None"; entries].join(", ")
    );
    let errors = check("test.star", &source).unwrap_err();
    assert_eq!(errors.len(), entries);
    assert!(
        errors
            .iter()
            .all(|e| e.snippet.chars().count() == SNIPPET_CHARS)
    );
    // Each snippet shows its own violation.
    for error in [&errors[0], &errors[entries / 2], &errors[entries - 1]] {
        assert!(error.snippet.contains("None"));
    }
    // A short line is kept whole.
    let short = check("test.star", "def main():\n    x = None\n").unwrap_err();
    assert_eq!(short[0].snippet, "    x = None");
}

#[test]
fn a_snippet_window_keeps_whole_characters() {
    // A long line of multi-byte characters, with the violation at its end.
    let source = format!("def main():\n    x = \"{}\" + None\n", "é".repeat(400));
    let errors = check("test.star", &source).unwrap_err();
    let snippet = &errors[0].snippet;
    assert_eq!(snippet.chars().count(), SNIPPET_CHARS);
    assert!(snippet.ends_with("+ None"));
}

#[test]
fn a_long_line_is_resolved_in_linear_time() {
    // Many valid statements on one line.
    let statements: Vec<String> = (0..20_000).map(|at| format!("v{at} = {at}")).collect();
    let valid = format!("def main():\n    {}\n", statements.join("; "));
    let started = std::time::Instant::now();
    passes(&valid);
    // Many violations on one line, each at its own column.
    let line = format!("    x = [{}]", vec!["None"; 20_000].join(", "));
    let invalid = format!("def main():\n{line}\n");
    let errors = check("test.star", &invalid).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let expected: Vec<u32> = line
        .match_indices("None")
        .map(|(at, _)| u32::try_from(at + 1).unwrap())
        .collect();
    let columns: Vec<u32> = errors.iter().map(|e| e.column).collect();
    assert_eq!(columns, expected);
}

#[test]
fn a_long_function_name_is_stored_once() {
    // A long caller and many calls from it: the call graph shares the name rather than copying
    // it per call.
    let name = "f".repeat(20_000);
    let source = format!(
        "def helper():\n    pass\n\ndef {name}():\n{}\ndef main():\n    {name}()\n",
        "    helper()\n".repeat(20_000)
    );
    let started = std::time::Instant::now();
    passes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    // A recursive nested def names only itself in the hint.
    let nested = format!("def {name}():\n    def g():\n        g()\n\ndef main():\n    pass\n");
    let errors = check("test.star", &nested).unwrap_err();
    let recursion = errors
        .iter()
        .find(|e| e.code == StarCode::UnboundedNotAllowed)
        .unwrap();
    assert!(recursion.hint.contains("`g`"));
    assert!(recursion.hint.len() < 100);
}

#[test]
fn a_chain_of_literal_keys_is_checked_in_linear_time() {
    let source = format!(
        "def main():\n    res = diag.request(0x22, {{}})\n    x = res{}\n",
        // As deep as the expression bound allows (2.8): the `=` and the index's bracket are one
        // level each.
        "[\"fields\"]".repeat(MAX_EXPRESSION_DEPTH - 2)
    );
    let started = std::time::Instant::now();
    passes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn many_dict_locals_and_many_loops_stay_cheap() {
    let count = 2_000;
    let mut source = String::from("def main():\n");
    for at in 0..count {
        source.push_str(&format!("    d{at} = {{\"a\": {at}}}\n"));
    }
    for _ in 0..count {
        source.push_str("    # @maxIterations 1\n    for i in range(1):\n        pass\n");
    }
    let started = std::time::Instant::now();
    passes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn many_loops_rebinding_a_dict_to_a_dict_stay_cheap() {
    let count = 2_000;
    let mut source = String::from("def main():\n");
    for at in 0..count {
        source.push_str(&format!("    d{at} = {{\"a\": {at}}}\n"));
    }
    for _ in 0..count {
        source.push_str("    # @maxIterations 1\n    for i in range(1):\n        d0 = {}\n");
    }
    let started = std::time::Instant::now();
    passes(&source);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[test]
fn many_nested_scopes_beside_many_locals_stay_cheap() {
    // Each nested def, lambda and comprehension is a scope; none copies the names around it.
    let count = 2_000;
    let mut source = String::from("def main():\n");
    for at in 0..count {
        source.push_str(&format!("    v{at} = {at}\n"));
    }
    for at in 0..count {
        source.push_str(&format!(
            "    def g{at}(x):\n        return x\n    f{at} = lambda y: y\n    c{at} = [z for z in range(1)]\n"
        ));
    }
    let started = std::time::Instant::now();
    let errors = check("test.star", &source).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(errors.len(), 3 * count);
}

#[test]
fn nested_defs_under_a_long_name_do_not_repeat_it() {
    // Nested defs are numbered, not named after the function they are in.
    let name = "f".repeat(20_000);
    let mut source = format!("def {name}():\n");
    for at in 0..20_000 {
        source.push_str(&format!("    def g{at}():\n        pass\n"));
    }
    source.push_str("\ndef main():\n    pass\n");
    let started = std::time::Instant::now();
    let errors = check("test.star", &source).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(errors.len(), 20_000);
}

#[test]
fn nested_defs_in_a_loop_are_walked_once() {
    // A chain of aliases makes the loop's fixpoint take many passes; the nested defs in it,
    // each holding another, are checked once and numbered once.
    let count = 512;
    let mut source = String::from("def main():\n    k = \"a\"\n");
    for at in 0..=count {
        source.push_str(&format!("    a{at} = 0\n"));
    }
    source.push_str("    # @maxIterations 2\n    for i in range(2):\n");
    for at in (0..count).rev() {
        source.push_str(&format!("        a{at} = a{}\n", at + 1));
    }
    source.push_str(&format!("        a{count} = {{\"a\": 1}}\n"));
    for at in 0..count {
        source.push_str(&format!(
            "        def g{at}():\n            def h():\n                pass\n"
        ));
    }
    source.push_str("        x = a0[k]\n");
    let started = std::time::Instant::now();
    let errors = check("test.star", &source).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let closures = errors
        .iter()
        .filter(|e| e.code == StarCode::ClosureNotAllowed)
        .count();
    assert_eq!(closures, 2 * count);
    assert!(
        errors
            .iter()
            .any(|e| e.code == StarCode::DynamicKeyNotAllowed)
    );
}

/// The nesting check is one pass over the call graph, however many sections and calls there are.
#[test]
fn many_sections_are_checked_in_linear_time() {
    let mut source = String::new();
    for at in 0..2_000 {
        source.push_str(&format!(
            "def h{at}():\n    diag.wait(1)\n\n# @section interruptible\ndef s{at}():\n"
        ));
        for callee in 0..at.min(20) {
            source.push_str(&format!("    h{callee}()\n"));
        }
        source.push_str("    pass\n\n");
    }
    source.push_str("def main():\n");
    for at in 0..2_000 {
        source.push_str(&format!("    s{at}()\n"));
    }
    let started = std::time::Instant::now();
    let checked = passes(&source);
    assert_eq!(checked.annotations.len(), 2_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

/// `main` with `line` as its body, which holds one link (the `=` of an assignment) besides its
/// expression.
fn in_main(line: &str) -> String {
    format!("def main():\n    {line}\n")
}

/// The codes of `source`, with no line.
fn bare_codes(source: &str) -> Vec<StarCode> {
    codes(source).into_iter().map(|(code, _)| code).collect()
}

fn too_deep(source: &str) -> bool {
    bare_codes(source) == [StarCode::NestingTooDeep]
}

#[test]
fn brackets_nest_to_the_bound() {
    for (open, close) in [
        ("(", ")"),
        ("[", "]"),
        ("{0: ", "}"),
        ("f(", ")"),
        ("f\"{", "}\""),
    ] {
        let at = |n: usize| in_main(&format!("x = {}1{}", open.repeat(n), close.repeat(n)));
        assert!(!too_deep(&at(MAX_BRACKET_DEPTH)), "{open}");
        assert!(too_deep(&at(MAX_BRACKET_DEPTH + 1)), "{open}");
    }
}

#[test]
fn expression_chains_reach_the_bound() {
    // Each `link` nests `per` levels; `overhead` counts the assignment's `=` and, for an index,
    // the level of its own bracket.
    let shapes: [(&str, &str, &str, usize, usize); 8] = [
        ("res", "[\"f\"]", "", 1, 2),
        ("res", ".f", "", 1, 1),
        ("1", " + 1", "", 1, 1),
        ("", "not ", "True", 1, 1),
        ("", "-", "1", 1, 1),
        ("", "1 if a else ", "1", 2, 1),
        ("", "lambda: ", "1", 1, 1),
        ("", "lambda a, b: ", "1", 1, 1),
    ];
    for (head, link, tail, per, overhead) in shapes {
        let at = |n: usize| in_main(&format!("x = {head}{}{tail}", link.repeat(n)));
        let most = (MAX_EXPRESSION_DEPTH - overhead) / per;
        assert!(!too_deep(&at(most)), "{link}");
        assert!(too_deep(&at(most + 1)), "{link}");
    }
}

#[test]
fn commas_separate_siblings() {
    let item = format!("1{}", " + 1".repeat(MAX_EXPRESSION_DEPTH / 2));
    let list = vec![item; 4].join(", ");
    assert!(!too_deep(&in_main(&format!("x = [{list}]"))));
    assert!(!too_deep(&in_main(&format!("x = f({list})"))));
}

#[test]
fn what_wraps_a_bracket_adds_to_the_depth_inside_it() {
    // Each closing bracket is wrapped by the links that follow it. One bracket level is left for
    // an index.
    let parens = MAX_BRACKET_DEPTH - 1;
    let wrapped = |operand: &str, link: &str, links: usize| {
        in_main(&format!(
            "x = {}{operand}{}",
            "(".repeat(parens),
            format!("{})", link.repeat(links)).repeat(parens)
        ))
    };
    // 1 for the `=`, then each parenthesis and its links.
    let fits = (MAX_EXPRESSION_DEPTH - 1) / parens - 1;
    for (operand, link) in [("1", " + 1"), ("res", "[\"f\"]"), ("res", ".f")] {
        assert!(!too_deep(&wrapped(operand, link, fits)), "{link}");
        assert!(too_deep(&wrapped(operand, link, fits + 1)), "{link}");
        assert!(
            too_deep(&wrapped(operand, link, MAX_EXPRESSION_DEPTH - 1)),
            "{link}"
        );
    }
}

/// `main` whose body nests `depth` blocks in all, the innermost holding `body`.
fn nested_blocks(depth: usize, body: &str) -> String {
    let mut source = "def main():\n".to_owned();
    for level in 1..depth {
        source += &format!("{}if a:\n", "    ".repeat(level));
    }
    source + &format!("{}{body}\n", "    ".repeat(depth))
}

#[test]
fn blocks_nest_to_the_bound() {
    assert!(!too_deep(&nested_blocks(MAX_BLOCK_DEPTH, "pass")));
    assert!(too_deep(&nested_blocks(MAX_BLOCK_DEPTH + 1, "pass")));
}

/// An `if` statement with `elifs` `elif` clauses in `main`, the last one's body holding `body`
/// (already indented).
fn elif_chain(elifs: usize, body: &str) -> String {
    let mut source = "def main():\n    if a:\n        pass\n".to_owned();
    for _ in 1..elifs {
        source += "    elif a:\n        pass\n";
    }
    source + &format!("    elif a:\n{body}    else:\n        pass\n")
}

#[test]
fn elif_chains_reach_the_bound() {
    assert!(!too_deep(&elif_chain(MAX_ELIF_CHAIN, "        pass\n")));
    let past = elif_chain(MAX_ELIF_CHAIN + 1, "        pass\n");
    assert!(too_deep(&past));
    // Inline bodies with `;` stay inside the chain.
    let mut inline = "def main():\n    if a: pass; pass\n".to_owned();
    inline += &"    elif a: pass; pass\n".repeat(MAX_ELIF_CHAIN + 1);
    assert!(too_deep(&inline));
    // Separate `if` statements do not add up.
    let two = elif_chain(MAX_ELIF_CHAIN, "        pass\n").replacen("def main():\n", "", 1);
    assert!(!too_deep(&format!("def main():\n{two}{two}")));
}

/// A file at every bound at once, in the costliest shapes: an `elif` chain whose last branch
/// nests blocks, the innermost holding nested brackets (`opening` each, which also adds
/// `links` links) around a `not` chain that fills the rest of the expression bound.
fn at_every_bound(opening: &str, links: usize) -> String {
    let expression = format!(
        "x = {}{}True{}",
        opening.repeat(MAX_BRACKET_DEPTH),
        "not ".repeat(MAX_EXPRESSION_DEPTH - 1 - (links + 1) * MAX_BRACKET_DEPTH),
        "]".repeat(MAX_BRACKET_DEPTH)
    );
    // The `elif` body is at depth 2; it nests the remaining blocks.
    let mut body = String::new();
    for level in 2..MAX_BLOCK_DEPTH {
        body += &format!("{}if a:\n", "    ".repeat(level));
    }
    body += &format!("{}{expression}\n", "    ".repeat(MAX_BLOCK_DEPTH));
    format!("a = 1\n{}", elif_chain(MAX_ELIF_CHAIN, &body))
}

/// Within the bounds, a file checks without overflow in a debug build, and its AST is dropped on
/// this test thread's default stack.
#[test]
fn a_file_at_every_bound_checks() {
    let checked = passes(&at_every_bound("[", 0));
    drop(checked);
    // Nested comprehensions cost the most stack per bracket; the subset rejects them, after
    // parsing.
    let comprehensions = bare_codes(&at_every_bound("[1 for a in ", 2));
    assert!(!comprehensions.is_empty());
    assert!(!comprehensions.contains(&StarCode::NestingTooDeep));
}

#[test]
fn a_hundred_thousand_levels_are_refused() {
    let chain = in_main(&format!("x = res{}", "[\"f\"]".repeat(100_000)));
    assert_eq!(bare_codes(&chain), [StarCode::NestingTooDeep]);
    let brackets = in_main(&format!(
        "x = {}1{}",
        "(".repeat(100_000),
        ")".repeat(100_000)
    ));
    let errors = check("test.star", &brackets).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, StarCode::NestingTooDeep);
    assert_eq!(
        (errors[0].line, errors[0].column),
        (2, 9 + MAX_BRACKET_DEPTH as u32)
    );
    assert!(errors[0].hint.contains("brackets"), "{}", errors[0].hint);
}

#[test]
fn a_source_past_the_size_bound_is_refused() {
    let at = |n: usize| format!("def main():\n    pass\n#{}", "a".repeat(n - 22));
    assert_eq!(at(MAX_SOURCE_BYTES).len(), MAX_SOURCE_BYTES);
    assert!(check("test.star", &at(MAX_SOURCE_BYTES)).is_ok());
    let errors = check("test.star", &at(MAX_SOURCE_BYTES + 1)).unwrap_err();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, StarCode::SourceTooLarge);
    assert_eq!(errors[0].line, 3);
}

#[test]
fn long_runs_of_one_character_lex_without_overflow() {
    // The lexer of an unoptimised `starlark_syntax` recursed per character of such runs.
    let spaces = format!(
        "def main():\n    x = 1{}\n",
        " ".repeat(MAX_SOURCE_BYTES - 30)
    );
    assert!(check("test.star", &spaces).is_ok());
    let continuations = format!("def main():\n    x = 1{}\n", " \\\n".repeat(300_000));
    assert!(check("test.star", &continuations).is_ok());
}

// ---------------------------------------------------------------- Flash plans (4.9, ADR-336)

/// A valid plan: its section, annotation, `@erase`, transfer and the mandatory preconditions.
const PLAN: &str = "\
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

/// The codes of `PLAN` with its `from` replaced by `to`.
fn plan_codes(from: &str, to: &str) -> Vec<(StarCode, u32)> {
    assert!(PLAN.contains(from), "{from}");
    codes(&PLAN.replace(from, to))
}

#[test]
fn a_valid_flash_plan_passes_with_its_annotations() {
    let checked = passes(PLAN);
    let span = |checked: &Checked, wanted: u32| {
        checked.annotations.iter().find_map(|a| match a {
            Annotation::FlashPlan { line, span, .. }
            | Annotation::Erase { line, span, .. }
            | Annotation::RecoveryRequired { line, span, .. }
                if *line == wanted =>
            {
                Some(*span)
            }
            _ => None,
        })
    };
    assert_eq!(
        checked.annotations[1],
        Annotation::FlashPlan {
            line: 2,
            def: 3,
            span: span(&checked, 2).unwrap(),
            session: 1,
            max_resumes: 3,
            recovery_never: false
        }
    );
    assert_eq!(
        checked.annotations[2],
        Annotation::Erase {
            line: 4,
            def: 3,
            span: span(&checked, 4).unwrap(),
            statement_at: PLAN.find("diag.routine").unwrap() as u32
        }
    );
    // Every attribute, in any order, and `@recoveryRequired` on a statement of the plan.
    let source = PLAN
        .replace("session=1", "recovery=never maxResumes=65535 session=7")
        .replace("flash_transfer(1)", "flash_transfer(7)");
    let checked = passes(&source);
    assert!(matches!(
        checked.annotations[1],
        Annotation::FlashPlan {
            session: 7,
            max_resumes: 65_535,
            recovery_never: true,
            ..
        }
    ));
    let source = PLAN.replace("    res =", "    # @recoveryRequired\n\n    res =");
    let checked = passes(&source);
    assert!(checked.annotations.iter().any(|a| matches!(
        a,
        Annotation::RecoveryRequired {
            line: 6,
            def: 3,
            ..
        }
    )));
    // A session of a constant, and a call in an `if`, are fine.
    let source = PLAN
        .replace("# @preconditions", "SESSION = 1\n\n# @preconditions")
        .replace("flash_transfer(1)", "flash_transfer(SESSION)")
        .replace("    write()", "    if True:\n        write()");
    passes(&source);
    // `diag.flash_transfer` outside a plan is the lowering's to refuse, not the checker's.
    passes("def main():\n    diag.flash_transfer(1)\n");
}

#[test]
fn flash_plan_annotations_are_checked() {
    use StarCode::{ArgumentOutOfRange as Range, BadAnnotation as Bad};
    let flash = |text: &str| plan_codes("# @flashPlan session=1", text);
    assert_eq!(flash("# @flashPlan"), [(Bad, 2)]);
    assert_eq!(flash("# @flashPlan maxResumes=2"), [(Bad, 2)]);
    assert_eq!(flash("# @flashPlan session=x"), [(Bad, 2)]);
    assert_eq!(flash("# @flashPlan session=1 session=2"), [(Bad, 2)]);
    assert_eq!(flash("# @flashPlan session=1 recovery=always"), [(Bad, 2)]);
    assert_eq!(
        flash("# @flashPlan session=1 recovery=never recovery=never"),
        [(Bad, 2)]
    );
    assert_eq!(flash("# @flashPlan session=1 maxResumes=0"), [(Range, 2)]);
    assert_eq!(
        flash("# @flashPlan session=1 maxResumes=65536"),
        [(Range, 2)]
    );
    assert_eq!(flash("# @flashPlan session=1 maxResumes=-1"), [(Range, 2)]);
    assert_eq!(flash("# @flashPlan session=-1"), [(Range, 2)]);
    // The maximum id is the reserved id of a plain procedure.
    assert_eq!(flash("# @flashPlan session=4294967295"), [(Range, 2)]);
    assert!(matches!(
        passes(&PLAN.replace("session=1", "session=1 maxResumes=65535")).annotations[1],
        Annotation::FlashPlan {
            max_resumes: 65_535,
            ..
        }
    ));
    // A `def` takes one.
    assert_eq!(
        flash("# @flashPlan session=1\n# @flashPlan session=1"),
        [(Bad, 3)]
    );
    // Placement: directly before a top-level `def`, alone at the start of its line.
    let nested = "\
def main():
    # @flashPlan session=1
    diag.wait(1)
";
    assert_eq!(codes(nested), [(Bad, 2)]);
    assert_eq!(
        codes("def f():\n    pass\n\n# @flashPlan session=1\nx = 1\ndef main():\n    f()\n"),
        [(Bad, 4)]
    );
    assert_eq!(
        plan_codes("def write():", "def write():  # @flashPlan session=1"),
        [(Bad, 3)]
    );
    // `@erase` and `@recoveryRequired` take no argument, stand before a statement, belong to
    // a plan function and to a top-level statement of it, and are given once.
    assert_eq!(plan_codes("# @erase", "# @erase now"), [(Bad, 4)]);
    assert_eq!(plan_codes("# @erase", "# @erase\n    # @erase"), [(Bad, 5)]);
    assert_eq!(plan_codes("# @erase", "# @recoveryRequired x"), [(Bad, 4)]);
    assert_eq!(
        plan_codes("    write()", "    # @erase\n    write()"),
        [(Bad, 10)]
    );
    assert_eq!(
        plan_codes("    write()", "    # @recoveryRequired\n    write()"),
        [(Bad, 10)]
    );
    assert_eq!(
        plan_codes(
            "    # @erase\n    diag.routine(0xFF00, 1, {})",
            "    if True:\n        # @erase\n        diag.routine(0xFF00, 1, {})"
        ),
        [(Bad, 5)]
    );
    assert_eq!(
        plan_codes(
            "    res =",
            "    # @recoveryRequired\n    # @recoveryRequired\n    res ="
        ),
        [(Bad, 7)]
    );
    assert_eq!(
        plan_codes(
            "    res = diag.flash_transfer(1)",
            "    diag.flash_transfer(1)  # @erase"
        ),
        [(Bad, 6)]
    );
}

#[test]
fn a_flash_plan_breaks_a_shape_rule() {
    use StarCode::{BadAnnotation as Bad, PlanShape as Shape};
    let call = "    write()";
    // Called never, twice, in a `for`, as a value, by another function or by a section.
    assert_eq!(plan_codes(call, "    pass"), [(Shape, 3)]);
    assert_eq!(plan_codes(call, "    write()\n    write()"), [(Shape, 11)]);
    assert_eq!(
        plan_codes(call, "    for i in range(2):\n        write()"),
        [(Shape, 11)]
    );
    assert_eq!(
        plan_codes(
            call,
            "    for i in range(2):\n        if i:\n            write()"
        ),
        [(Shape, 12)]
    );
    assert_eq!(plan_codes(call, "    x = write()"), [(Shape, 10)]);
    let helper = |annotation: &str| {
        let pre = "# @preconditions voltage externalSupply ignition\ndef main():\n    write()";
        plan_codes(
            pre,
            &format!(
                "{annotation}def helper():\n    write()\n\n# @preconditions voltage externalSupply \
                 ignition\ndef main():\n    helper()"
            ),
        )
    };
    assert_eq!(helper(""), [(Shape, 9)]);
    // A section function that calls the plan breaks the nesting rule of 4.5 as well.
    assert_eq!(
        helper("# @section interruptible\n"),
        [(Bad, 10), (Shape, 10)]
    );
    // A plan that calls a plan: the call is a shape error, and no nesting one, since the caller's
    // expansion inlines what it calls.
    let two = "\
# @section uninterruptible
# @flashPlan session=1
def first():
    # @erase
    diag.routine(0xFF00, 1, {})
    diag.flash_transfer(1)
    second()

# @section uninterruptible
# @flashPlan session=2
def second():
    # @erase
    diag.routine(0xFF00, 1, {})
    diag.flash_transfer(2)

# @preconditions voltage externalSupply ignition
def main():
    first()
";
    assert_eq!(codes(two), [(Shape, 7), (Shape, 14)]);
    // `main` cannot be a plan, and a plan returns nothing.
    assert_eq!(
        codes("# @flashPlan session=1\ndef main():\n    pass\n"),
        [(Shape, 1)]
    );
    assert_eq!(
        plan_codes("    res =", "    if True:\n        return\n    res ="),
        [(Shape, 7)]
    );
}

#[test]
fn a_flash_plan_needs_its_section_and_one_transfer() {
    use StarCode::PlanShape as Shape;
    let transfer = "    res = diag.flash_transfer(1)";
    assert_eq!(
        plan_codes("# @section uninterruptible expected=1000\n", ""),
        [(Shape, 2)]
    );
    assert_eq!(
        plan_codes("uninterruptible expected=1000", "interruptible"),
        [(Shape, 3)]
    );
    passes(&PLAN.replace("uninterruptible", "recoveryRequired"));
    assert_eq!(plan_codes(transfer, "    pass"), [(Shape, 3)]);
    assert_eq!(
        plan_codes(transfer, &format!("{transfer}\n    diag.flash_transfer(1)")),
        [(Shape, 7)]
    );
    // The session must be the annotation's, through a constant too.
    assert_eq!(
        plan_codes(transfer, "    res = diag.flash_transfer(2)"),
        [(Shape, 6)]
    );
    assert_eq!(
        codes(
            &PLAN
                .replace("# @preconditions", "OTHER = 4\n\n# @preconditions")
                .replace("flash_transfer(1)", "flash_transfer(OTHER)")
        ),
        [(Shape, 6)]
    );
    // The transfer is a top-level statement: not in an `if`, not inside a larger expression.
    assert_eq!(
        plan_codes(transfer, "    if True:\n        diag.flash_transfer(1)"),
        [(Shape, 7)]
    );
    assert_eq!(
        plan_codes(transfer, "    res = diag.flash_transfer(1)[\"raw\"]"),
        [(Shape, 6)]
    );
}

#[test]
fn erase_and_recovery_required_in_a_flash_plan() {
    use StarCode::PlanShape as Shape;
    let erase = "    # @erase\n    diag.routine(0xFF00, 1, {})\n";
    let transfer = "    res = diag.flash_transfer(1)\n";
    // `@erase` precedes one `diag.routine` call, before the transfer.
    assert_eq!(
        plan_codes(erase, "    # @erase\n    diag.wait(1)\n"),
        [(Shape, 4)]
    );
    assert_eq!(
        plan_codes(erase, "    # @erase\n    diag.request(0x11, {})\n"),
        [(Shape, 4)]
    );
    assert_eq!(
        plan_codes(&format!("{erase}{transfer}"), &format!("{transfer}{erase}")),
        [(Shape, 5)]
    );
    // Not the erase of two statements.
    assert_eq!(
        plan_codes(
            erase,
            "    # @erase\n    diag.routine(0xFF00, 1, {})\n    # @erase\n    diag.routine(0xFF01, 1, {})\n"
        ),
        [(Shape, 6)]
    );
    // `@erase` assigned to a name is a single call too.
    passes(&PLAN.replace("    diag.routine", "    r = diag.routine"));
    // `@recoveryRequired` and `recovery=never` exclude each other, and a plan whose section is
    // `recoveryRequired` takes neither.
    let marked = "    # @recoveryRequired\n    res = diag.flash_transfer(1)";
    passes(&PLAN.replace("    res = diag.flash_transfer(1)", marked));
    passes(&PLAN.replace("session=1", "session=1 recovery=never"));
    assert_eq!(
        codes(
            &PLAN
                .replace("    res = diag.flash_transfer(1)", marked)
                .replace("session=1", "session=1 recovery=never")
        ),
        [(Shape, 6)]
    );
    let required = PLAN.replace("uninterruptible", "recoveryRequired");
    assert_eq!(
        codes(&required.replace("session=1", "session=1 recovery=never")),
        [(Shape, 2)]
    );
    assert_eq!(
        codes(&required.replace("    res = diag.flash_transfer(1)", marked)),
        [(Shape, 6)]
    );
}

#[test]
fn a_procedure_with_a_flash_plan_names_its_mandatory_preconditions() {
    use StarCode::PlanShape as Shape;
    // Missing altogether: reported at `main`.
    assert_eq!(
        plan_codes("# @preconditions voltage externalSupply ignition\n", ""),
        [(Shape, 8)]
    );
    // Missing a kind: reported at the annotation, which names it.
    let source = PLAN.replace("voltage externalSupply ignition", "voltage ignition engine");
    let errors = check("test.star", &source).unwrap_err();
    assert_eq!(
        errors.iter().map(|e| (e.code, e.line)).collect::<Vec<_>>(),
        [(Shape, 8)]
    );
    assert!(
        errors[0].hint.contains("missing: externalSupply"),
        "{}",
        errors[0].hint
    );
    // A procedure without a plan needs none of them.
    passes("def main():\n    pass\n");
}

#[test]
fn flash_plan_codes_are_named_as_specified() {
    assert_eq!(StarCode::PlanShape.as_str(), "STAR_PLAN_SHAPE");
    assert_eq!(
        StarCode::UnknownFlashSession.as_str(),
        "STAR_UNKNOWN_FLASH_SESSION"
    );
    assert_eq!(
        serde_json::to_string(&StarCode::PlanShape).unwrap(),
        "\"STAR_PLAN_SHAPE\""
    );
    assert_eq!(
        serde_json::to_string(&StarCode::UnknownFlashSession).unwrap(),
        "\"STAR_UNKNOWN_FLASH_SESSION\""
    );
}

#[test]
fn a_parameter_hides_a_constant_in_the_session_comparison() {
    use StarCode::PlanShape as Shape;
    // The parameter, not the constant, is what the transfer names: not compared.
    let shadowed = "\
SESSION = 1

# @section uninterruptible
# @flashPlan session=2
def write(SESSION):
    # @erase
    diag.routine(0xFF00, 1, {})
    diag.flash_transfer(SESSION)

# @preconditions voltage externalSupply ignition
def main():
    write(2)
";
    passes(shadowed);
    // Without the parameter the constant is compared.
    assert_eq!(
        codes(
            &shadowed
                .replace("write(SESSION)", "write()")
                .replace("write(2)", "write()")
        ),
        [(Shape, 8)]
    );
}

#[test]
fn a_nested_recovery_required_marker_is_a_bad_annotation() {
    let nested = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    for i in range(2):\n        # @recoveryRequired\n        diag.wait(i)\n    res = diag.flash_transfer(1)",
    );
    assert_eq!(codes(&nested), [(StarCode::BadAnnotation, 7)]);
}

#[test]
fn the_session_comparison_reads_signs_big_literals_and_chains() {
    use StarCode::PlanShape as Shape;
    let with = |constants: &str, arg: &str| {
        codes(
            &PLAN
                .replace(
                    "# @preconditions",
                    &format!("{constants}\n# @preconditions"),
                )
                .replace("flash_transfer(1)", &format!("flash_transfer({arg})")),
        )
    };
    assert_eq!(with("", "+1"), []);
    assert_eq!(with("", "+2"), [(Shape, 6)]);
    assert_eq!(
        with("", "99999999999999999999999999999999999999999999"),
        [(Shape, 6)]
    );
    assert_eq!(with("", "4294967296"), [(Shape, 6)]);
    assert_eq!(with("A = 1\nB = A\nC = B\n", "C"), []);
    assert_eq!(with("A = 2\nB = A\n", "B"), [(Shape, 6)]);
    // A later constant, or anything computed, is not compared.
    assert_eq!(with("B = A\nA = 2\n", "B"), []);
    assert_eq!(with("", "1 + 1"), []);
}

#[test]
fn a_transfer_in_a_function_the_plan_calls_is_refused() {
    use StarCode::PlanShape as Shape;
    let direct = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    helper()",
    ) + "\ndef helper():\n    diag.flash_transfer(1)\n";
    assert_eq!(codes(&direct), [(Shape, 14)]);
    let transitive = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    helper()",
    ) + "\ndef helper():\n    inner()\n\ndef inner():\n    diag.flash_transfer(1)\n";
    assert_eq!(codes(&transitive), [(Shape, 17)]);
    // A section function the plan calls, too.
    let section = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    helper()",
    ) + "\n# @section interruptible\ndef helper():\n    diag.flash_transfer(1)\n";
    assert!(
        codes(&section).contains(&(Shape, 15)),
        "{:?}",
        codes(&section)
    );
}

#[test]
fn the_erase_and_transfer_statements_hold_one_diag_call() {
    use StarCode::PlanShape as Shape;
    // An outer `diag.routine` with another `diag` call among its arguments is two operations.
    assert_eq!(
        plan_codes(
            "diag.routine(0xFF00, 1, {})",
            "diag.routine(0xFF00, 1, {\"x\": diag.routine(0xFF01, 1, {})})"
        ),
        [(Shape, 4)]
    );
    assert_eq!(
        plan_codes(
            "diag.routine(0xFF00, 1, {})",
            "diag.routine(0xFF00, diag.wait(1), {})"
        ),
        [(Shape, 4)]
    );
    // The transfer's argument holds no other `diag` call either.
    let errors = codes(&PLAN.replace(
        "flash_transfer(1)",
        "flash_transfer(len(diag.log(1, \"\")))",
    ));
    assert_eq!(errors, [(Shape, 6)]);
    // Nor a call to a function of the program, which the expansion would inline into the
    // statement; built-ins and methods are fine.
    let helper = "\ndef args():\n    return 1\n";
    let erase = PLAN.replace(
        "diag.routine(0xFF00, 1, {})",
        "diag.routine(0xFF00, args(), {})",
    ) + helper;
    assert_eq!(codes(&erase), [(Shape, 4)]);
    let transfer = PLAN.replace("flash_transfer(1)", "flash_transfer(args())") + helper;
    assert_eq!(codes(&transfer), [(Shape, 6)]);
    let lambda = PLAN.replace(
        "diag.routine(0xFF00, 1, {})",
        "diag.routine(0xFF00, (lambda: 1)(), {})",
    );
    assert!(codes(&lambda).contains(&(Shape, 4)), "{:?}", codes(&lambda));
    passes(&PLAN.replace(
        "diag.routine(0xFF00, 1, {})",
        "diag.routine(0xFF00, min(1, 2), {})",
    ));
    // The plain forms still pass.
    passes(PLAN);
}

/// A section function a plan reaches is inlined in the plan's expansion (ADR-336 item 3), so it
/// nests in no other section; outside the plan the nesting rule still holds.
#[test]
fn a_section_function_inlined_in_a_plan_does_not_nest() {
    let helper = "\n# @section interruptible\ndef helper():\n    diag.wait(1)\n";
    let inlined = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    helper()",
    ) + helper;
    passes(&inlined);
    // Through a plain function the plan calls, too.
    let through = PLAN.replace(
        "    res = diag.flash_transfer(1)",
        "    res = diag.flash_transfer(1)\n    relay()",
    ) + helper
        + "\ndef relay():\n    helper()\n";
    passes(&through);
    // A section that calls `helper` outside every plan still nests.
    let outside = inlined.clone()
        + "\n# @section interruptible\ndef other():\n    helper()\n"
        + "\ndef extra():\n    other()\n";
    let outside = outside.replace("    write()\n", "    write()\n    extra()\n");
    assert!(
        codes(&outside).contains(&(StarCode::BadAnnotation, 20)),
        "{:?}",
        codes(&outside)
    );
}
