# ADR-340: HMI Forms and Record Templates Are Encoded `HmiForm`s, Answered as a Status Byte and Text

**Date:** 2026-10-21
**Status:** Accepted
**Affects:** `diag-ir` (new `hmi` module, `Op::HmiRequestFromStack`, `IR_SCHEMA_VERSION` 17 to 18,
`Program::validate`, `VmError`, `dry_run::Script`), `diag-frontend` (the lowering of `diag.hmi`
and `diag.record` and of the answer's keys), `agent` (`host.rs`, `runner.rs`, `policy.rs`),
`shared-proto` (`EventBody::HmiRequest` and the answer command), `api/asyncapi.yaml`,
`api/openapi.yaml`, Starlark subset 2.5, 3.1, 3.2, 3.4 and 4.6, design 8.2.4 and 8.6; ADR-229,
ADR-233 (items 4 and 7), ADR-254, ADR-304, ADR-317, ADR-328, ADR-330, ADR-335

## Context

`Op::HmiRequest { form }` and `Op::RecordInput { template }` exist since ADR-233: each sends one
constant-pool entry to the host (`DiagHost::hmi_request` / `record_input`, both
`(inquiry, &[u8]) -> Result<Option<Vec<u8>>, _>`), reports `WaitingOn::Hmi` while the host returns
`Ok(None)`, and pushes the answer as `Bytes` (ADR-233 item 7). Nothing defines what the constant
holds or what the answer's bytes mean, so:

- the frontend rejects `diag.hmi` and `diag.record` (`STAR_API_NOT_AVAILABLE`,
  `crates/diag-frontend/src/star/lower.rs`), and subset 3.2 says only that the constant must carry
  the form and its constant `params`, and that run-time parameters need a stack variant;
- the agent's host answers both with `HostError::Unsupported`, and the runner turns
  `Waiting(Hmi)` into `JobError::Unanswerable`;
- the dry run (ADR-328) scripts raw answer bytes (`Script::form`, `Script::record`);
- the agent-to-server event is `EventBody::HmiRequest { job_id, request_id, form: String }`
  (`crates/shared-proto/src/lib.rs`), the stream schema sends `hmiRequest: { requestId, templateId }`
  and takes back `hmiResponse: { jobId, requestId, values: object }` (`api/asyncapi.yaml`), and the
  Web API's `POST /v1/jobs/{jobId}/hmi/{requestId}` body is an untyped object (`api/openapi.yaml`).

What the two sides know differs. The converter sees only the Starlark call: a form or template id
(`str`) and, for `diag.hmi`, a dict literal with string keys. HMI forms are screen definitions and
record templates are their own extension point, both server-side data resolved by id (design 9.2,
4.3.1; `schemas/record-template.schema.json` with item types number, choice, boolean, text and
attachment, choice values as strings, and pass criteria). The converter has no catalog of either,
so it cannot know a form's answer type, and the procedure's constants cannot be checked against
the form at ingestion.

The VM's values are `I64`, `F64`, `Bool` and `Bytes`; a dict exists only at ingestion (subset 2.5).
The precedent for a structured value at run time is ADR-317: a `diag.request` response is its bytes,
and each literal key lowers to the instructions that read it (`ok`, `nrc`, `raw`, `fields`).

The run-time constraints are ADR-233's: the request bytes must be rebuilt identically on every
poll and after a resume from the journal at the same instruction, under the same inquiry number
(the state's `steps`), so the host can tell a poll from a new inquiry; every instruction either
completes or leaves the state unchanged; a `VmError` is terminal and a host error resumable
(ADR-288).

Open choices: the byte encoding the VM hands the host; how constant and run-time parameters share
it, and which instruction carries the run-time ones (ADR-233's append rule); the Starlark type of
each call's value and how a decline, a timeout and a cancel surface; which checks the lowering,
`Program::validate`, the VM, the agent and the server each make; and the implementation split.

## Decision

1. **One encoding for both calls: a postcard-encoded `diag_ir::hmi::HmiForm`.**
   ```rust
   pub struct HmiForm { pub id: String, pub params: Vec<(String, HmiValue)> }
   pub enum HmiValue { Int(i64), Bool(bool), Text(String) }
   ```
   `id` is the form id or the record template id (the `id` of
   `schemas/record-template.schema.json`), 1 to 128 bytes of UTF-8 (`MAX_HMI_ID_LEN`). `params` holds
   at most 32 pairs (`MAX_HMI_PARAMS`), in the source order of the Starlark dict literal; a key is an
   ASCII identifier (`[A-Za-z_][A-Za-z0-9_]*`) of 1 to 64 bytes, unique within the form. The whole
   encoding is at most 16 KiB (`MAX_HMI_FORM_LEN`). A record template's `params` is empty in this
   version. `hmi::encode` and `hmi::decode` are the one codec; `decode` enforces every bound.
   Postcard is already the IR's codec (`Op`, `VmState`), its output is canonical once the pair order
   is fixed (it is: source order), a typed struct validates the shape by construction, and the VM
   needs the same encoder for item 2. JSON appears once, where the agent hands the form to the
   server (item 5).
2. **The constant-pool entry is an `HmiTemplate`, and run-time values come from the stack through
   a new instruction.** `HmiTemplate` is `HmiForm` with `params: Vec<(String, HmiParam)>`,
   `HmiParam::Const(HmiValue) | Runtime`, under the same bounds and codec.
   - `HmiRequest { form }` and `RecordInput { template }` keep their meaning: the template has no
     `Runtime` slot, and the VM sends its `HmiForm` (a `RecordInput` template has no params).
   - `Op::HmiRequestFromStack { form: u32 }` is appended after `ReadInput` (ADR-233 item 10),
     `IR_SCHEMA_VERSION` 17 to 18. It decodes the template at `form`, pops one value per `Runtime`
     slot, the first slot being the deepest, maps `I64` to `Int`, `Bool` to `Bool` and `Bytes` that
     are valid UTF-8 to `Text`, fills the slots, encodes the `HmiForm` with the codec of item 1 and
     calls `DiagHost::hmi_request(inquiry, &bytes)`. The trait does not change. Before the host is
     called, `check_primitive` checks the template decodes (`VmError::BadHmiForm`), every operand's
     type (`TypeMismatch`; invalid UTF-8 is `InvalidUtf8`), the encoded length
     (`VmError::HmiFormTooLong`) and the room for the answer (`check_room(n, 1)`), so
     `Vm::current_op` keeps ADR-233 item 3's promise that an intent is journaled only for a request
     the next step sends. On `Ok(None)` the step is `Waiting(WaitingOn::Hmi)` with the state
     unchanged, operands included, as `WaitFromStack` leaves its duration; so every poll and every
     resume at the instruction rebuilds the same bytes under the same inquiry number. On
     `Some(answer)` the operands are replaced by the answer. It is a diagnostic primitive
     (journaled, one-instruction `Section`, `Safe` by subset 4.6's default for `hmi`), and the
     agent's policy gives it the class of `HmiRequest` (none).
   - `Program::validate` decodes the template of every `HmiRequest`, `RecordInput` and
     `HmiRequestFromStack` and refuses a `Runtime` slot under the first two, or params under
     `RecordInput` (`ProgramError::BadHmiForm { pc }`), so ingestion finds a bad constant as it finds
     a bad `Fail` code (ADR-280); `check_primitive` repeats the decode, which is cheap under the 16
     KiB bound, so a damaged program fails before the host is called.
3. **The answer is one status byte followed by UTF-8 text, and in Starlark a value read through
   the literal keys `ok`, `timed_out` and `value`.** The host returns `[status] ++ text`: status 1
   means the operator answered, 0 that the operator declined the form or record, and 2 that the
   HMI timeout of design 5.6 expired first (item 6); with 0 and 2 `text` is empty; the whole answer is at most 16 KiB (`MAX_HMI_ANSWER_LEN`). That shape is the host's
   contract, and the VM pushes the answer as it pushes every other primitive's, without checking
   it after the call: ADR-233 item 3 (every VM error comes before the host call, so no answer is
   discarded) holds unchanged. The reads below are total over any bytes: an empty answer, or one
   whose first byte is not 1, reads as `ok == False`, and `value` is whatever follows the first
   byte. Text that is not UTF-8 fails only where a later instruction needs UTF-8, in that
   instruction's own pre-host check, after the answer is stored.
   The value of `diag.hmi(...)` and of `diag.record(...)` has a type of its own in the lowering
   (`Ty::HmiAnswer`), the answer bytes, read only by a string literal key as a response is
   (ADR-317):
   - `ans["ok"]` is a `bool`: `PushI64(0); PushI64(1); Slice; PushBytes(k); CmpEq` with `k` the
     pooled constant `[0x01]` (the sequence `response_ok` uses);
   - `ans["timed_out"]` is a `bool`: the same sequence with the constant `[0x02]`;
   - `ans["value"]` is a `str`: `PushI64(1); PushI64(i64::MAX); Slice`; empty when `ok` is false.
   Another key is `STAR_UNSUPPORTED_SYNTAX`, a non-literal key `STAR_DYNAMIC_KEY_NOT_ALLOWED`, and
   any other use of the answer (its truth value, an operator, a built-in, passing it on)
   `STAR_NOT_LOWERABLE`, as for a response.
   What `value` holds is the server's to fill, by the kind of form: empty for a confirmation; the
   entered text for a text input; the number as decimal text, normalized by the server, for a
   number input; the chosen option's `value` string for a choice; and for a record template the
   server's overall judgment of the record's pass criteria, `ok` or `ng`, or empty when the
   template has none. A decline gives `ok == False` and an empty `value` for every kind, and a
   timeout the same with `timed_out == True`. A job cancel is never an answer: the instruction
   stays waiting until the runner takes the cancel (item 6).
4. **The lowering checks (subset 3.1, 3.2).** `diag.hmi(form_id, params)` and
   `diag.record(template_id)` take exactly those arguments (`STAR_UNSUPPORTED_SYNTAX` otherwise).
   The id is a constant `str` (`STAR_NON_CONSTANT_ARGUMENT` when known only at run time,
   `STAR_UNSUPPORTED_SYNTAX` for a constant of another type) of 1 to 128 bytes
   (`STAR_ARGUMENT_OUT_OF_RANGE`). `params` is a dict literal whose keys are string literals that
   are identifiers of at most 64 bytes, unique, at most 32 of them (`STAR_ARGUMENT_OUT_OF_RANGE`
   for a bound, `STAR_UNSUPPORTED_SYNTAX` for a key that is not such a literal). A value that is a
   constant `int`, `bool` or `str` becomes a `Const` slot; a value known only at run time whose
   one inferred type (ADR-304) is `int`, `bool` or `str` becomes a `Runtime` slot, and its code is
   emitted in source order before the instruction, as `LogFromStack`'s message is; a value of any
   other type, constant or not (`float`, `bytes`, a list, a dict, a response), is
   `STAR_UNSUPPORTED_SYNTAX`, and a run-time value with more than one possible type
   `STAR_NOT_LOWERABLE`. A template that encodes to more than 16 KiB is
   `STAR_ARGUMENT_OUT_OF_RANGE`. A call with no `Runtime` slot lowers to `HmiRequest`, one with
   any to `HmiRequestFromStack`; `diag.record` lowers to `RecordInput`. The default idempotency
   stays `Safe` (4.6) and the primitive gets its one-instruction `Section` (ADR-332).
5. **The agent's host and the server.** The host keeps, per instance and in memory, a map from
   inquiry number to the request's state, as `wait_elapsed` keeps its timers: the first poll of an
   inquiry generates a `request_id` (UUIDv7, design 4.4), decodes the `HmiForm`, sends
   `EventBody::HmiRequest { job_id, generation, request_id, inquiry, kind, form }`, with `inquiry`
   the VM's inquiry number, `kind` `Form` or
   `Record`, `generation` the job's ownership generation (ADR-229: every job-scoped message
   carries it, and the server applies one only under the current owner and generation), and
   `form` typed (`id` and `params`, which serialize as JSON on the wire, an `Int` as a
   decimal string so that no value outside the range a browser holds exactly is rounded before
   the operator sees it), and returns `Ok(None)`;
   later polls return `Ok(None)` until the answer command `HmiAnswer { job_id, generation,
   request_id, ok, value }` arrives, then encode item 3's bytes and return `Some`, closing the
   inquiry. The agent takes an answer or refusal command only when its job, generation and request
   id name the open inquiry, and drops any other as stale. The host holds the inquiry's HMI
   deadline (item 6) and moves the inquiry from open to timed out under the same lock that takes
   commands, so exactly one of the two wins: an answer taken before the deadline is returned even
   if the poll comes later, and one that arrives after it is dropped as late (the server keeps it
   as audit data) and the next poll returns the timed-out answer. The agent
   checks only the shape (`value` is text of at most 16 KiB minus one, empty when `ok` is false)
   when the command arrives, before any poll can return it; a command of another shape is refused
   to the server and the inquiry stays open, so the host never hands the VM a malformed answer;
   the server, which has the form or template, validates the operator's input against it
   (choices, criteria, required items) before it answers, and for a record stores the record
   itself (design 4.3.1); only the judgment reaches the agent. A new host instance, which every
   restart creates, opens a new `request_id` for the same inquiry number: the server supersedes
   the job's open inquiry, and a late answer to a superseded id is audit data only. Because a
   restart may repeat the primitive (it is `Safe`, ADR-335) after the server stored a record but
   before the agent's checkpoint after it, a record must never count twice. Within one run the
   inquiry number is unique, since `steps` only grows. Across runs the agent tells the server where
   each run starts: before a run's first instruction (a first run or any restart) it sends
   `EventBody::HmiRunStart { job_id, generation, from_inquiry }` with the inquiry number of the
   state it starts from, 0 for a run from the start (ADR-335). Everything the abandoned run did at
   or after that number is what the new run repeats, so the server supersedes the job's records
   whose inquiry number is at or above `from_inquiry`, which stay as audit data only, and keeps
   those below it, which the resumed state already holds. The job's active records are therefore
   exactly those of the steps the final run did not repeat plus those it made, with no duplicate
   whatever branch a repeated read takes. The answer itself is never reused. A server
   refusal (an id the server does not have, params the form cannot take) is a command of its own,
   `HmiRefused { job_id, generation, request_id, reason }`: the next poll closes the inquiry and
   returns `HostError::HmiRefused`, a resumable host error and not an answer, so a resume at the
   instruction opens a new inquiry. Whenever an inquiry ends without the operator's answer, the
   host sends `EventBody::HmiWithdrawn { job_id, generation, request_id }`, so the server takes
   the form off the screen: when it closes the inquiry with the timed-out answer (item 6), and when
   the run ends while the inquiry is still open (a cancel taken at the waiting instruction, the
   HMI timeout's included, or any other end of the run), before the job's final state is
   reported. The stream and Web API schemas change accordingly: `hmiRequest`
   carries the generation, `kind`, `id` and `params`; `hmiResponse` and the `POST
   .../hmi/{requestId}` body are `{ ok: boolean, value: string }`; the refusal, the withdrawal and the run start
   are messages of their own.
6. **The runner polls an HMI wait as it polls a timer.** `Waiting(WaitingOn::Hmi)` reports
   `JobState::AwaitingHmi`, sleeps `wait_poll` and steps again, keeping the keep-alive and the
   cancel check; `JobError::Unanswerable` remains for `SeedKey` only. When the HMI timeout of
   design 5.6 expires, the runner raises the job's cancel flag and the host closes the inquiry with
   the timed-out answer (status 2) on its next poll, telling the server the inquiry is withdrawn
   (item 5).
   Outside every uninterruptible and recovery-required section the waiting instruction is an
   interruptible point, so the runner takes the cancel before that poll and the job ends there;
   the inquiry is still open, so the host withdraws it as the run ends (item 5).
   Inside such a section ADR-330 holds the cancel, so the instruction completes with the timed-out
   answer, the procedure runs on to the section's end (it may test `timed_out` and `diag.fail`),
   and the held cancel is taken at the next interruptible point, as design 5.6 asks. Without that
   answer a wait in such a section could never end. A run whose HMI timeout fired never ends as
   Completed: when the procedure reaches its end while that cancel is still held, which ADR-330
   item 2 would let finish, the job ends as cancelled by the HMI timeout instead, as it would have
   at an interruptible point. Every instruction ran, and the job's outcome still says the operator
   never answered. The timeout outlives a restart: the runner commits a journal record of it
   (`Record::HmiTimedOut`) before the host returns the timed-out answer, so no checkpoint can
   hold that answer without the record, and a restart that finds the record starts its run with
   the cancel already raised and the timeout as its reason. The same rules then apply to the
   resumed run, whether it resumes from a checkpoint or starts again from the first instruction,
   so a repeated form answered after the restart cannot let the job end as Completed. A procedure that ends by `diag.fail` or an error while the cancel is held ends
   that way, as ADR-330 item 2 says. Where the timeout's value comes from (the job
   instruction or the agent's settings) is a separate decision.
7. **The dry run keeps its contract.** `Script::form` and `Script::record` still take answer
   bytes, and gain `Script::form_answer(ok, text)`, `record_answer(ok, text)` and
   `hmi_timed_out()` that build them;
   the report decodes `DryRunEvent::Hmi::form` and `RecordInput::template` as `HmiForm`s. Raw
   bytes of another shape are pushed as they are and read as item 3 says, so a test can script a
   host that breaks the contract.

## Alternatives rejected

- **Canonical JSON bytes in the constant pool and on the host boundary.** No schema for a form
  request exists to reuse (`record-template.schema.json` describes the template, not the request);
  the VM would need a JSON reader and writer (serde_json is not a `diag-ir` dependency) and this ADR
  would have to define a canonical form (key order, number text, escaping); the stack variant would
  parse and re-emit JSON on every poll; and the agent builds the server's JSON anyway, so JSON would
  be produced twice.
- **CBOR.** A new dependency, the same canonical-encoding question, and no consumer that wants it.
- **A stack variant that pops one `Bytes` the lowering builds with `Concat`.** The IR has no
  integer-to-text or escaping instruction, so a run-time `int`, or a `str` with a quote in it,
  cannot be serialized by bytecode, and the VM could not check the result without parsing it.
- **Keys in the instruction (`HmiRequestFromStack { form, keys }`).** Variable-length data in
  `Op`, where every variant is fixed-size; the template constant holds the same information.
- **A second trait method taking typed values (`hmi_request_values(inquiry, form, &[Value])`).**
  Changes every `DiagHost`, moves the merge and its determinism to each host, and makes the poll
  identity depend on two inputs.
- **Typed answers (the host returns `I64` or `Bool`, pushed as such).** Breaks ADR-233 item 7
  (primitives push `Bytes`), and the Starlark type would depend on the form definition the
  converter cannot see, so the lowering could not give the value one type (ADR-304).
- **A plain `str` or `bool` as the call's value.** A text form could not tell an empty answer
  from a decline, and a record could not report its judgment.
- **A numeric key (`ans["number"]` as `int`) now.** Needs a bytes-to-integer instruction or a
  decode of some sixty instructions per read; the number reaches the procedure as decimal text,
  and a `BytesToInt`-like instruction, which would also serve `int(s)`, is a decision of its own.
- **`float` and `bytes` parameters.** JSON carries neither NaN and infinities nor bytes; `int(x *
  10)` and hex text cover the cases seen so far; both can be appended to `HmiValue` later (a
  schema version bump, no format change).
- **Treating a server refusal as `ok == False`.** Hides a deployment fault behind an operator
  decline.
- **Leaving the instruction waiting on an HMI timeout and relying on the cancel alone.** Inside
  an uninterruptible or recovery-required section ADR-330 holds the cancel until the instruction
  completes, so the wait would never end and the job would never reach the interruptible point
  design 5.6 stops at.
- **Refusing `diag.hmi` and `diag.record` inside such sections.** A flash plan may need the
  operator (to switch the ignition, for one), and the section is where that happens.
- **Reporting a timeout as a plain decline.** A procedure could not tell an operator's refusal
  from an absent operator.
- **Keying the server-side inquiry by (job, inquiry number) across restarts.** A replay from the
  start (ADR-335) reaching the same step count would receive a previous run's answer without the
  operator; a request id per host instance costs at most one repeated question after a restart.

## Consequences

- Both calls lower, so `diag.flash_transfer` is the one `diag` call left without a lowering
  (ADR-336). The two backlog items on `diag.hmi` / `diag.record` and on the stack variant close.
- `IR_SCHEMA_VERSION` becomes 18: a runtime of 17 cannot decode `HmiRequestFromStack`, and the
  version a server matches against `capabilities` (9.5) changes, as for every appended
  instruction (ADR-233). `VmError` gains `BadHmiForm` and `HmiFormTooLong` (both raised before the host call),
  `ProgramError` gains `BadHmiForm`. ADR-330 item 2 is amended for a cancel an HMI timeout
  raised (item 6). ADR-233's status annotation records the appended
  instruction, and its item 7 is read with this ADR: the constant is an encoded `HmiTemplate`.
  The IR golden files change in their version, and in their code if `procedure.star` gains a
  `diag.hmi` and a `diag.record` with scripted answers.
- No `DiagHost` implementation changes its signature; test hosts and the fuzz target compile as
  they are, and the dry run's report gains decoded forms.
- Limits: 16 KiB per form and per answer, 32 parameters, 128-byte ids. A form keeps its answer in
  the VM state (and its journaled checkpoints) like any `Bytes` until the variable is dropped.
- A procedure cannot pass a `float` or `bytes` to a form, nor read an entered number as an `int`;
  it compares `value` with `==` against string constants (subset 2.5, `CmpEq` on bytes). Each is a
  follow-up item, as is a `diag.record` with prefilled parameters.
- The converter does not know the server's form and template catalogs, so an unknown id is found
  only when the server ingests the package or, failing that, as a run-time host error. Follow-up:
  the converter lists the form and template ids each procedure names, beside its text and unit
  catalogs, so the server can refuse a package that names an id it does not have.
- Implementation split. **PR A** (`diag-ir`, `diag-frontend`, docs): the `hmi` module and its
  bounds, `HmiRequestFromStack`, the VM and `Program::validate` checks, the dry-run helpers, the
  lowering of both calls and of the three keys with tests against a scripted host (a constant form,
  a form with constant and run-time parameters in mixed order, a record, a decline, a timed-out answer, a resume at
  the waiting instruction rebuilding identical bytes), subset 2.5, 3.1, 3.2 and 3.4, and ADR-233's
  and ADR-254's status annotations; design 8.6 records the contract with this ADR. **PR B** (`agent`, `shared-proto`, `api/`): the host's inquiry map and
  `HostError::HmiRefused`, the runner's `AwaitingHmi` polling, the event and command shapes, the
  schema files, and design 8.2.4's agent paragraph, which says HMI requests and record input are
  not supported yet, with tests against a fake server channel; the server's rendering, validation and
  record storage stay with the server's milestone. PR B and the follow-ups above are backlog
  items.
