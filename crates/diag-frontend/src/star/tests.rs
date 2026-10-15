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
fn only_one_comment_marker_starts_an_annotation() {
    let checked = passes(
        "def main():\n    ## @idempotency safe\n    ## @unknown\n    # @idempotency checkState\n    \
         diag.wait(1)\n",
    );
    assert_eq!(
        checked.annotations,
        [Annotation::Idempotency {
            line: 4,
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

#[test]
fn a_section_pairs_by_the_indentation_of_its_lines() {
    // The `#` columns line up, but the lines are at different indentation.
    assert_eq!(
        codes(
            "def main():\n    x = 1        # @section interruptible\n    if x:\n        y = 2    # @endsection\n"
        ),
        [(StarCode::BadAnnotation, 2), (StarCode::BadAnnotation, 4)]
    );
    // An inline opener and a standalone closer on lines of one indentation pair.
    passes(
        "def main():\n    x = 1  # @section interruptible\n    diag.wait(1)\n    # @endsection\n",
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
