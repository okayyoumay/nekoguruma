# ADR-318: A Job Carries Its Variant's Document

**Date:** 2026-10-16
**Status:** Accepted (items 1, 3 and 4 amended by ADR-325)
**Affects:** `agent` (`src/document.rs`, `src/runner.rs`: `Job`, `check_job`,
`JobError::NoDeclarations` and `JobError::Undeclared`; `src/host.rs`, `src/main.rs`:
`ngr-agent run --document`), `diag-ir` (`check_decode_fields`), design 8.2.4,
`crates/agent/docs/ngr-agent.md`; ADR-316, ADR-307

## Context

ADR-316 lets the VM decode a response field through the declaration part it is stepped with,
but the agent's job runner stepped the VM with none, so a program that decodes failed with
`NoDeclarations`. The runner has to get a variant's declarations to every step, the steps of a
restart included. Design 8.2.6 ships one file per variant and procedure, holding the
variant's declarations and the procedure that was lowered against them (ADR-307, ADR-311): the
service and field ids a program decodes are that variant's.

Three questions were open: how a job pairs a program with declarations, when a program that
cannot decode is refused, and whether the journal records the document a restart must use.

## Decision

1. **A job is a program, or a document.** `agent::Job` holds the program and, optionally, the
   variant's document (`agent::JobDocument`, checked by `load_ecu_document`). A job with a
   document is made only from the document (`Job::from_document`), whose procedure is the
   program, so a program never decodes with another variant's declarations. A program alone
   (`From<Program>`) has no declarations. `ngr-agent run` takes `--document` beside
   `--program`.
2. **Refused before anything is sent.** `check_job`, which every entry point runs before the
   link opens, refuses a program that decodes without a document (`NoDeclarations`). It also
   refuses one that decodes a field the document does not declare as a top-level field without
   a nested layout (`Undeclared`, through `diag_ir::check_decode_fields`). ADR-316 already
   checks these at the instruction, but they depend only on the declarations, and a VM error is
   terminal (ADR-288): found at the instruction, after a write, it would end a job that a
   corrected document could no longer resume.
3. **The host carries the document.** `WorkerHost` holds the job's document, and the runner's
   step loop reads it through the `agent::document::Declarations` trait, whose default is no
   document. Every path that steps the VM, the restart's replay and redo included, goes through
   that loop, so none can step without it.
4. **The journal does not record the document.** A restart gets the document from its caller,
   as it gets the program: the journal identifies neither. The caller must pass the job's
   document again.

## Consequences

- ADR-316's note that the runner steps without declarations, and its first consequence, no
  longer hold.
- A restart given another document, or another program, is not detected. A document changed
  between the runs could make a resumed write job decode its responses differently. Recording
  the document's content hash (design 8.2.6) in the journal at its creation and comparing it on
  a restart, as the VIN and the intended software version are compared, is left as follow-up
  work.
- Each run of the VM checks the document again when it loads the variant, a cost bounded by
  ADR-296's limits.
