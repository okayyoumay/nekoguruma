//! The conversion's dry run of each lowered procedure on built answers (ADR-391).

use diag_ir::Failure;
use diag_ir::decode::{DecodeError, DecodedField};
use diag_ir::dry_run::DryRunError;

use super::*;

/// The rejection of `source` as the only procedure of the example, or `None` when it converts.
fn rejection(source: &str) -> Option<Vec<StarError>> {
    procedures(with(example(), "p.star", source))
        .err()
        .map(|rejected| rejected.errors)
}

/// The one error of a rejected `source`.
fn stopped(source: &str) -> StarError {
    let mut errors = rejection(source).expect("the procedure is refused");
    assert_eq!(errors.len(), 1, "{errors:?}");
    errors.remove(0)
}

#[test]
fn a_procedure_that_finishes_is_accepted() {
    assert_eq!(rejection(LOG), None);
}

#[test]
fn an_authors_fail_is_accepted() {
    assert_eq!(rejection(FAIL), None);
}

#[test]
fn a_loop_past_its_limit_is_accepted() {
    // The built session is 0, so the bound is 10 and the loop passes its limit of 3: the
    // lowering's own failure, LOOP_LIMIT_EXCEEDED, ends the run.
    let source = "def main():\n\
        \x20   r = diag.request(0x22, {\"did\": 0xF186})\n\
        \x20   n = r[\"fields\"][\"session\"] + 10\n\
        \x20   # @maxIterations 3\n\
        \x20   for i in range(n):\n\
        \x20       diag.log(1, \"a\")\n";
    assert_eq!(rejection(source), None);
}

#[test]
fn a_built_answer_that_does_not_decode_is_accepted() {
    // The built answer of lastResetReason selects no case, so decoding it for `reason` fails.
    let source = "def main():\n\
        \x20   r = diag.request(0x22, {\"did\": 0xFD03})\n\
        \x20   if r[\"fields\"][\"reason\"] == 1:\n\
        \x20       diag.log(1, \"a\")\n";
    assert_eq!(rejection(source), None);
}

#[test]
fn an_index_out_of_range_is_refused_at_its_line() {
    let source = "def main():\n\
        \x20   dtcs = diag.read_dtc(0x09)\n\
        \x20   records = dtcs[\"fields\"][\"dtcs\"]\n\
        \x20   diag.log(1, \"a\")\n\
        \x20   if records[0][\"code\"] == 1:\n\
        \x20       diag.log(1, \"b\")\n";
    let error = stopped(source);
    assert_eq!(error.code, star::StarCode::DryRunStopped);
    assert_eq!(error.code.as_str(), "STAR_DRY_RUN_STOPPED");
    assert_eq!((error.file.as_str(), error.line), ("p.star", 5));
    assert!(
        error
            .hint
            .starts_with("the dry run on answers built from the declarations stopped: "),
        "{}",
        error.hint
    );
    assert!(error.hint.contains("index out of range"), "{}", error.hint);
    // Both variants of the example give it, so no variant is named.
    assert!(!error.hint.contains("(in variant"), "{}", error.hint);
}

#[test]
fn a_run_that_does_not_end_in_the_budget_is_refused() {
    let source = "def main():\n\
        \x20   for i in range(2000000):\n\
        \x20       pass\n";
    let error = stopped(source);
    assert_eq!(error.code, star::StarCode::DryRunStopped);
    assert!(
        error
            .hint
            .contains("did not end within 1000000 instructions"),
        "{}",
        error.hint
    );
}

#[test]
fn security_access_a_form_a_record_and_preconditions_run_on_the_built_defaults() {
    let source = "# @preconditions voltage externalSupply ignition engine vehicleSpeed\n\
        def main():\n\
        \x20   unlock = diag.security_access(0x01)\n\
        \x20   if unlock[\"ok\"]:\n\
        \x20       diag.log(1, \"unlocked\")\n\
        \x20   confirm = diag.hmi(\"confirm\", {\"title\": \"Confirm\"})\n\
        \x20   if not confirm[\"ok\"]:\n\
        \x20       diag.fail(\"DECLINED\", \"\")\n\
        \x20   record = diag.record(\"final_check\")\n\
        \x20   if record[\"ok\"]:\n\
        \x20       diag.log(1, \"recorded\")\n";
    assert_eq!(rejection(source), None);
}

#[test]
fn a_refusal_that_not_every_variant_gives_names_the_variants_that_do() {
    let error = star::at_position(
        star::StarCode::DryRunStopped,
        "p.star",
        "def main():\n",
        1,
        1,
        "the dry run on answers built from the declarations stopped: x".to_owned(),
    );
    let merged = merge(vec![("v1", vec![error.clone()]), ("v2", vec![])], 2);
    assert_eq!(merged.len(), 1);
    assert!(
        merged[0].hint.ends_with("x (in variant v1)"),
        "{}",
        merged[0].hint
    );
}

// ---------------------------------------------------------------- The verdict itself

/// The variant of the example's first row and a program to hand to the verdict.
fn verdict(report: &DryRunReport) -> Result<(), StarError> {
    let definition =
        resolve(read_files(with(example(), "p.star", LOG)).expect("reads")).expect("resolves");
    let bytes = encode_ecu_document(&declarations(&definition)[0]);
    let loaded = load_ecu_document(&bytes).expect("loads");
    let variant = loaded.variant().expect("a variant");
    let procedures = lower_procedures(&definition).expect("lowers");
    let mut program: Program = postcard::from_bytes(&procedures[0].programs[0]).expect("a program");
    program.source_map = vec![diag_ir::SourceSpan {
        pc: 0,
        file: 0,
        line: 2,
        column: 5,
    }];
    dry_run_verdict(
        "p.star",
        "def main():\n    diag.log(1, \"a\")\n",
        &program,
        &variant,
        report,
    )
}

fn report(end: DryRunEnd, events: Vec<DryRunEvent>) -> DryRunReport {
    DryRunReport {
        events,
        end,
        steps: 1,
        pc: 0,
    }
}

/// The id and short name of the first declared service with a positive response.
fn some_service() -> (u32, String) {
    let definition = resolve(read_files(example()).expect("reads")).expect("resolves");
    let bytes = encode_ecu_document(&declarations(&definition)[0]);
    let loaded = load_ecu_document(&bytes).expect("loads");
    let variant = loaded.variant().expect("a variant");
    let services = variant.services().expect("services");
    let service = services
        .iter()
        .find(|s| s.positive().is_some())
        .expect("a service");
    (
        service.id(),
        service.short_name().expect("a name").to_owned(),
    )
}

fn request_event(
    name: &str,
    built: bool,
    decoded: Option<Result<Vec<DecodedField>, DecodeError>>,
) -> DryRunEvent {
    DryRunEvent::Request {
        request: vec![0x22],
        service: name.to_owned(),
        answer: Vec::new(),
        built,
        decoded,
    }
}

#[test]
fn the_verdict_accepts_a_finish_and_every_failure() {
    assert_eq!(verdict(&report(DryRunEnd::Finished, vec![])), Ok(()));
    let failure = Failure {
        code: "X".to_owned(),
        detail: Vec::new(),
    };
    assert_eq!(verdict(&report(DryRunEnd::Failed(failure), vec![])), Ok(()));
}

#[test]
fn the_verdict_accepts_an_undecodable_answer_only_when_it_was_built() {
    let (service, name) = some_service();
    let undecodable = || DryRunEnd::Vm(VmError::Undecodable { service });
    let bad = || Some(Err(DecodeError::Empty));
    let built = report(undecodable(), vec![request_event(&name, true, bad())]);
    assert_eq!(verdict(&built), Ok(()));
    // A scripted answer, an answer that decoded, another service's answer and no answer at all
    // do not explain it.
    let scripted = report(undecodable(), vec![request_event(&name, false, bad())]);
    let decoded = report(
        undecodable(),
        vec![request_event(&name, true, Some(Ok(vec![])))],
    );
    let other = report(undecodable(), vec![request_event("other", true, bad())]);
    let none = report(undecodable(), vec![]);
    for refused in [scripted, decoded, other, none] {
        let error = verdict(&refused).expect_err("refused");
        assert_eq!(error.code, star::StarCode::DryRunStopped);
        assert!(error.hint.contains("does not decode"), "{}", error.hint);
    }
}

#[test]
fn the_verdict_refuses_host_errors_and_the_rest() {
    let ends = [
        DryRunEnd::Host(DryRunError::ReportTooLarge),
        DryRunEnd::Host(DryRunError::NotScripted {
            what: "form answer",
        }),
        DryRunEnd::Vm(VmError::IndexOutOfRange(-3)),
        DryRunEnd::Waiting(diag_ir::WaitingOn::Hmi),
        DryRunEnd::StepLimit,
    ];
    for end in ends {
        let error = verdict(&report(end, vec![])).expect_err("refused");
        assert_eq!(error.code, star::StarCode::DryRunStopped);
        assert_eq!((error.line, error.column), (2, 5));
    }
}
