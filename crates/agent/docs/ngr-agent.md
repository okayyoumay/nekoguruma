# `ngr-agent`

`ngr-agent` is the binary of the `agent` crate (design 3.3). It has one command, which runs a
single IR program on a VCI for local end-to-end runs and tests; it does not connect to the
server or take jobs from it.

## `ngr-agent run`

```sh
ngr-agent run --vci <name> --program <file> [--workers <dir>] [--locks <dir>] [--tx-id <hex>] [--rx-id <hex>]
```

| Option | Meaning |
|---|---|
| `--vci` | J2534 v04.04 library name, as the worker service resolves it (registry key on Windows, `library_path` entry in the service's `config.toml`) |
| `--program` | IR program file: a `diag_ir::Program` serialized as JSON |
| `--workers` | Directory of worker builds, laid out as `<dir>/<ABI name>/j2534-0404-service[.exe]` (design 7.3). Default: `workers` next to the `ngr-agent` executable |
| `--locks` | The device's lock directory for the job's guards ("Job guards" below), as an absolute path. Default: `locks` next to the `ngr-agent` executable. Lock files are opened read-only, so a file another user created still locks. The agent creates a missing directory writable by its owner only, so a device whose agents run as several users needs it prepared beforehand: writable by all of them, and with the sticky bit (or an equivalent ACL) so that no user can delete another's lock files (ADR-257). It must also be readable by every agent user, since the per-vehicle lock lists it, and on Unix support hard links, with which it creates its files (ADR-262). On Unix a directory that group or others may write without the sticky bit, or without being readable by them, is refused |
| `--tx-id`, `--rx-id` | Physical request and response CAN IDs in hex, with or without `0x`. Default `7E0` / `7E8`. Only 11-bit IDs: the link does not set the CAN ID format |

The link is UDS on ISO 15765 at 500 kbit/s (`LinkConfig::iso15765`), with the CAN IDs from the
command line rather than from the program.

Steps (`agent::check_program`, `agent::guards::JobGuards`, `agent::launch::launch_j2534_worker`, then `agent::run_program`):

1. Read the program and check it (`agent::check_program`): a program whose schema version or
   size this VM does not accept, one whose restart declaration `Program::validate` refuses
   (ADR-245), or one with a request this build may never send (a release build sends only
   read-only requests; ADR-235 item 8, ADR-247), fails here,
   before any worker starts. A bad operand, such as a missing constant, fails only when the VM
   reaches it, after the worker has started.
2. Take the job's guards from the `--locks` directory, named by `--vci` ("Job guards" below):
   the per-VCI lock, plus the reprogramming slot when the program writes (`policy::writes`).
   The run waits here while another job on the device holds them.
3. Resolve the VCI name the way each worker build would resolve it on its side
   (`j2534_0404_registry`): a `library_path` entry in `config.toml` under that build's
   architecture key, else at api level, else the build's registry view. The first build whose
   library header (`worker_host::abi::detect_file`) matches the build's own ABI is chosen, so
   the header that selects the build belongs to the file that build loads (ADR-240).
   - Windows x64 agent: the x64 build (`x86_x64` key, 64-bit registry view) first, then the x86
     build (`x86` key, 32-bit view, `KEY_WOW64_32KEY`). A VCI registered in both views runs on
     the x64 build.
   - Windows x86 agent: the x86 build only, so a 64-bit library is not found.
   - Linux: one lookup without an architecture key; the registry does not apply, so the
     library needs a `library_path` entry, and the build follows the header's ABI.
4. Pick the service build for that ABI with `WorkerLayout::find`. A missing build fails as
   `UNSUPPORTED_ABI`.
5. Launch the service with the library name and the ABI's default `long_size` (design 7.1.2;
   always the default, whatever a registration definition declares), provision its
   auth key and connect the gRPC client (design 7.4).
6. Run the program under the request policy and the default job limits (ADR-235), then stop
   the worker and release the guards. The stop request may take up to 2 s to be answered; a
   worker that has not exited 5 s after that is killed and reported on stderr. When the job's link was not confirmed closed and the worker
   cannot be stopped either, the run fails with "the worker may still hold the VCI", and the
   locks stay held until the process exits (ADR-258). The policy is read-only, except that a debug build may send any request once
   the worker's VCI has identified itself as `sim-vci` (ADR-247); a program that needs more
   than the VCI allows is refused after the link opens, before its first instruction runs.
   On that permission the `FlashTransfer` instruction also works (ADR-250); `SecurityAccess`
   stays refused, since an agent without a server has no key source (design 8.10, ADR-259).

## Data transfer

`FlashTransfer` sends one TransferData request (service 0x36) per instruction and requires a
positive response that echoes the block sequence counter; a negative response or a wrong echo
fails the job (ADR-250). The instruction's `block` operand is constant per instruction, so the
host ignores it and keeps its own count of the blocks of the transfer: the counter is 1 for the
first block after a RequestDownload (0x34, a `ServiceRequest` answered positively) sent through
the host, rises by one per confirmed block and continues at 0 after 0xFF (ISO 14229-1:2026
clause 14.4). Only a positive 0x34 begins a transfer. RequestUpload (0x35), RequestTransferExit
(0x37), RequestFileTransfer (0x38), DiagnosticSessionControl (0x10) and ECUReset (0x11) end it
whatever the response, and so does any failed block: a negative TransferData response, a wrong
echo or no response (a new RequestDownload is needed; a same-counter retry could leave a gap).
A `FlashTransfer` with no tracked transfer is an error and sends nothing, and a
`ServiceRequest` for 0x36 is refused, both before the link opens (`check_program`) and by
the host. `Program::validate` refuses a plan with 0x10, 0x11, 0x35 or 0x38 between its
RequestDownload and RequestTransferExit, or with a jump from the transfer back to the
RequestDownload or before it, so a valid program does not end its transfer before the
RequestTransferExit (ADR-250). `WorkerHost::transfer_block_index` gives the number of
confirmed blocks (`Some(0)`: started, none yet), which the write-job journal records
(ADR-244); it is a `u64`, so the journaling runner converts it for `commit_block` and never
commits 0 (ADR-252).

## Output and exit status

| Exit status | Meaning | Output |
|---|---|---|
| 0 | The program ran to its end | The final `diag_ir::VmState` as one line of JSON on stdout. A negative response is a value on the stack, not a failure |
| 1 | Reading the program, launching the worker or the job failed, the worker may still hold the VCI after a failed link close, or the final state holds a non-finite float (infinity or NaN), which JSON cannot represent | The reason on stderr |
| 2 | Invalid command line | The error and the usage on stderr |

The worker's own log goes to stderr.

## Configuration file

The agent and the worker read the same `config.toml` only if both binaries were built with the
same configuration root (`vci-service-config`'s `config-root-*` features). The default roots
agree. With `config-root-exe-dir`, the worker reads the file next to its own build under
`--workers`; with `config-root-win-program-files`, a 32-bit worker sees `Program Files (x86)`.
In both cases the agent can resolve a different library than the worker loads; build the
agent and the workers with the same root.

## Write-job journal

The `agent` library's `journal` module keeps the write-job journal (design 5.5, ADR-244): one
append-only file, `{job_id}.g{generation}.journal`, per job and ownership generation, in a
directory the caller passes. Every commit is synced before it returns. Only the job's writer opens
the journal for writing: `Journal::create` and `Journal::open` take an exclusive OS lock on the
sidecar `{job_id}.g{generation}.journal.lock` and hold it while the journal is open, so a second
writer gets `JournalError::InUse` (ADR-255). The lock goes with the process, so a crashed run
never blocks the next one. Other readers use `Journal::read`, which takes no lock and never
changes the file. The journal only records; committing an intent marker before the request it guards, and stopping the job when a
commit fails, are the job runner's duties.

`agent::run_program_journaled` is the runner that keeps it (ADR-252). It takes a
`JournalSetup` (directory, job key, the `ServiceSources` table for the ECU's identity, and
the VIN the job targets, which a first run records as the journal's first record and a
restart compares against, and the software version the job intends to write, recorded right
after it in the same creating write and compared the same way, ADR-268) and the job's guards ("Job guards"
below), which it returns with the result, and,
for a program with a flash recovery plan, creates the journal once the link is open and the
policy allows the program, before anything is sent to the ECU; a program without a plan keeps
none. A journal that already exists ends the job with nothing sent: a job that ran before goes
on through `resume_program_journaled` ("Restart entry" below). Every run from instruction 0, a first
run or a plain start on an existing journal, commits the VM state it starts from as its first
record (the run start, ADR-272), at the run's first step count and before anything is sent, so
the journal marks where each such run began. The restart's replay commits none: it starts from
the entry state the journal already holds as its newest ("Replay to the erase" below). Each time execution arrives at a plan's boundary, before that
instruction runs, it commits:

- at the entry, once per job and before its first transfer: the hardware part number and
  software version, read as raw field bytes (`inputs::read_field_bytes`) through the sources
  the program declares. A software version answered with the plan's declared "no valid
  application" response is not recorded, and the job goes on;
- at the erase: the transfer-start marker;
- at RequestTransferExit: its marker;
- at the first diagnostic primitive at or past a plan's recovery-required point that is neither
  of those: a request intent, so a crash before its response still places the interruption
  there (ADR-253).

When an instruction completes, it commits the block of a `FlashTransfer`, under the host's
running index (`TransferProgress`, ADR-250); a step record for every diagnostic primitive inside
a plan's range; the VM state on the record of a step that brings execution to a plan's entry
(a job that starts at the entry has its run start instead); and the
post-transfer completion with the step that reaches the plan's post-transfer end. A commit that
fails, or a declared identity source that gives no value (`JobError::IdentityUnreadable`, or
`JobError::IdentityRead` when the read cannot be sent), ends the job before the next
instruction runs, so a guarded request is never sent without its marker and nothing is erased
without the identity.

`ngr-agent run` uses `run_program` and keeps no journal; it sends read-only requests, or any
request to `sim-vci` in a debug build (ADR-247).

## Restart classification

`agent::restart::classify` reads a job's journal with the program and decides, without
contacting anything, how the job goes on (ADR-253). The interruption point is the latest, by
step count, of the last completed step, the two transfer markers and the last request intent.

| Journal | Decision |
|---|---|
| none (`NotFound`), program without a plan | plain start |
| none (`NotFound`), program with a plan | `OnSiteInterventionRequired` (the journal is created before anything is sent) |
| unreadable (corrupt, unknown format version, another job, I/O) | `OnSiteInterventionRequired` |
| interruption point at or past a plan's recovery-required point and before its end, or inside a `RecoveryRequired` section the journal can place it in (inside a plan, on the step into a plan's entry, or at a completed plan's end); a point no later than the last post-transfer step of a completed transfer counts as at that plan's end | `OnSiteInterventionRequired` |
| no transfer-start marker | plain start |
| transfer-start marker | `Restart`, for the plan of the marker's stage, with the VM state at its entry; `OnSiteInterventionRequired` if that plan never allows a restart |

A restart's entry state is the newest state the journal holds, a run start included, so a plain
start never inherits an earlier run's state. Only a journal that holds no state (written before
the run-start record) falls back to the program's initial state, for an entry at instruction 0.
The run start names no request, so it does not move the interruption point; the step count
counts it. It must decode, pass `Vm::check_state` and stand at the
plan's entry; otherwise, or for a stage the program does not declare, the decision is
`OnSiteInterventionRequired`. Its step count continues after the journal's last record
(`restart::next_steps`), so the restart's records come after the old ones.

## Restart entry

`agent::resume_program_journaled` takes the same arguments as `run_program_journaled`,
guards included, for a job that ran before (ADR-255), and returns the guards with its result.
Once the link is open and the policy allows the program, and before anything is sent to the
ECU, it opens the job's journal (`Journal::open`) and classifies it.

| Decision | What the runner does |
|---|---|
| plain start | runs the program from its start on the same journal; its step count starts at `restart::next_steps`, after the journal's last record, and the identity is read again. A program without a plan runs without a journal, when it has none or one that records no transfer |
| `OnSiteInterventionRequired` | ends in `JobError::OnSiteInterventionRequired` with the classification's reason; nothing is sent |
| `Restart` | runs `restart::check_before_ecu`, then the gates of `restart::check_gates` (which promote the guards to the per-vehicle lock once the VIN matches, ADR-263), then the teardown of `restart::teardown` (ADR-264), then the default-session confirmation of `restart::confirm_default_session` (ADR-265). An ECU that cannot be confirmed ends the job in `JobError::OnSiteInterventionRequired(DefaultSessionNotConfirmed)`. Then `restart::check_identity` (step 3a) reads the VIN and the hardware identity again: a different one ends the job in `IdentityMismatch`, one that cannot be established in `OnSiteInterventionRequired(IdentityNotEstablished)`, and a matching VIN promotes the guards again (the lock's fallback when step 2 could not read it). Then `restart::check_state` (step 3b-2) reads the software version and decides between redoing the transfer and the read-back verification (`restart::check_state` below): a version nobody wrote ends the job in `OnSiteInterventionRequired(UnexpectedSoftwareVersion)`, one that cannot be established in `OnSiteInterventionRequired(SoftwareVersionNotEstablished)`. When the state check decides to redo the transfer, `restart::check_reentry` (step 4a) checks every declared precondition again through its default-session source: one that does not hold ends the job in `OnSiteInterventionRequired(PreconditionNotMet)`. When step 4a passes, the program's steps from the plan's entry boundary up to, not including, its erase run again from `RestartPoint::entry_state` (step 4b-2, ADR-273; `replay to the erase` below), and then `restart::check_before_erase` (step 4b-3) checks every declared precondition once more through its programming-session source: one that does not hold ends the job in `OnSiteInterventionRequired(PreconditionNotMetBeforeErase)` with no erase sent. Otherwise, and after that check, it ends in `OnSiteInterventionRequired(RestartOrderUnavailable)` carrying the teardown's outcome, the confirmation and the state check, because the erase and the read-back verification do not run in the agent; the gates' reads, at most one ECUReset, the F186 reads, step 3a's two reads, step 3b-2's version reads, step 4a's precondition reads, the replayed steps and step 4b-3's precondition reads reach the ECU |

`check_before_ecu` is the part of ADR-229 item 2 step 1 that an agent without a server makes:

1. The resume limit. A stage whose count in the journal has reached the plan's `max_resumes`
   ends in `ResumeLimitReached`.
2. The supply voltage, when the program declares a voltage range. It is read from the VCI as
   `RuntimeInput::SupplyVoltageMillivolts`, whatever sources the program declares, since this
   step uses no ECU service. A reading outside the range, or none, ends in `SupplyVoltage`.
3. The resume count, incremented and committed with no attempt key.

A failed check counts no resume, and a cancel stops it before the voltage read, right after it
(whatever the read gave) and before the commit. Each crash during a recovery therefore consumes one attempt, and repeated crashes stop
at the limit. A second resume of a job whose journal another run holds ends in
`JobError::Journal(JournalError::InUse)` before the classification, with nothing sent. The start
deadline (a server's job instruction carries it) and a server reservation are not part of this
entry.

Before the classification, a resume whose `JournalSetup::vin` differs from the target VIN the
journal recorded at the first run (none included) ends in
`OnSiteInterventionRequired(TargetVinDiffers)`, with nothing sent and no resume counted
(ADR-261).

The same holds for `JournalSetup::intended_software_version`, the raw bytes of the procedure's
software-version field: a resume whose value differs from the one the journal recorded at the
first run (none included, on either side) ends in
`OnSiteInterventionRequired(IntendedVersionDiffers)`, checked right after the VIN and before the
classification, with nothing sent and no resume counted (ADR-268). Both records belong to the
journal's creation prefix: the target VIN only first, the intended version only first or right
after the VIN, each at most once; a file that breaks this is corrupt. An empty intended
version is refused, since it would match an ECU that answers with an empty field. Like the VIN record, the
version record is appended without a format version change.

`check_gates` is ADR-229 item 2 step 2 before the teardown (ADR-261). It reads only, and stops
at the first gate that does not hold:

1. The VIN, through the program's VIN source, against the VIN the job names (the journal's
   target VIN). A well-formed VIN (17 characters, each a digit or an upper-case letter other
   than I, O and Q) other than the job's ends the job in `JobError::IdentityMismatch`. No
   answer, a negative response, text that is not a well-formed VIN, or a job that names no
   well-formed VIN (which reads nothing) gives `PassiveOnly(VinNotEstablished)`.

   A VIN that matches promotes the job's guards to the per-vehicle lock at once (design 8.2.5,
   8.8; ADR-263): before the hardware identity read, so no ECU request lies between the match
   and the lock. The job waits while another job holds that vehicle, retrying at the job's
   `wait_poll`; the guard slot is not locked during the wait (the guards are taken out of it and
   put back, also on a panic), and a cancel ends the job in `JobError::Cancelled` with the guards
   back without the vehicle. Any other failure of the lock (`GuardError` other than a cancel)
   ends it in `JobError::VehicleLock`, with only ReadDataByIdentifier requests sent so far, and
   an empty slot in `JobError::GuardsMissing`. A VIN that is not established or differs takes no
   lock. The lock stays in the guards, which come back with the result
   (`JobGuards::holds_vehicle`); guards that already hold the job's vehicle go on at once.
2. The ECU's hardware part number, against the bytes the journal recorded before the erase. A
   different one gives `PassiveOnly(HardwareIdentityDiffers)`, one that cannot be read (or no
   recorded value) `PassiveOnly(HardwareIdentityNotEstablished)`. It aborts only in the later
   check in the default session.
3. Each declared safety precondition (voltage, external supply, ignition, engine, vehicle
   speed), from its default-session source and, when that gives no value, from a different
   programming-session source, since the ECU's session is not known yet. A value outside the
   declared range, or none (no source, an unmapped one, no answer), gives
   `PassiveOnly(Precondition(kind))`.

When all hold the decision is `ResetAllowed`: the gates do not rule a reset out, and the
teardown still applies the journal's exclusions (none after RequestTransferExit was journaled,
none on the completed path). A failure to use the worker during a read counts
as a read that gave no value. Neither the error nor the log carries a VIN. A cancel stops the
gates before and after each read.

`restart::teardown` is step 2b-1 (ADR-264), run on the gates' decision. In this order:

1. The journal's exclusions. Post-transfer steps journaled complete (the completed path) give
   `Teardown::CompletedPath`: no reset and no wait, since the default-session confirmation of
   step 2b-2 comes first (ADR-265). A RequestTransferExit intent without that completion gives
   `Teardown::Passive(TransferExitJournaled)`.
2. A `PassiveOnly` gate gives `Teardown::Passive(Gate(reason))`.
3. Otherwise one ECUReset (hardReset, positive response required) is sent. A positive response
   gives `Teardown::Reset`, with no wait: the ECU's startup time and the confirmation belong to
   step 2b-2 (ADR-265). A negative response gives `Passive(ResetRefused { nrc })`; a failed or unanswered
   request, a final response-pending code, or any other answer gives
   `Passive(ResetOutcomeUnknown)`.

A passive teardown sends nothing further (when it follows a refused or unknown ECUReset, that
reset was the last request; the agent runs no TesterPresent, so there is none to stop) and
waits the flash session's `session_timeout_millis + teardown_margin_millis`, in steps of the
job's `wait_poll`; a cancel during the wait ends the job in `JobError::Cancelled`.
`RestartOrderUnavailable` carries the outcome.

`restart::confirm_default_session` is step 2b-2 (ADR-265), run on the teardown's outcome. One
attempt waits the flash session's `ecu_startup_millis` (sending nothing, in steps of
`wait_poll`, a cancel ending the job in `Cancelled`), then reads DID F186 every `wait_poll`
until the ECU confirms its default session or `confirmation_window_millis` (counted from the
end of the startup wait) has passed; at least one read is made, and a cancel is checked before
every wait step and every read. Only a positive response with the value `01` confirms; a
negative response, no answer, another session value or any other answer is a failed read,
retried while the window lasts. When an attempt fails:

- after `Teardown::Reset` or `Teardown::CompletedPath`, where no passive teardown has run, the
  agent runs the passive teardown (the same wait as above) and makes one more attempt, startup
  wait included; if that confirms, the result is `Confirmation { after_passive_retry: true }`;
- after `Teardown::Passive`, where the wait has run already, the failure is final at once.

A failure is `JobError::OnSiteInterventionRequired(DefaultSessionNotConfirmed { flash_session,
teardown })`: someone on site must check the ECU. The F186 reads put no VIN in a
log message or a result.

`restart::check_identity` is step 3a (ADR-229 item 2 step 3), run after a confirmation. It
reads in the default session and sends nothing but ReadDataByIdentifier through the declared
sources, with a cancel check at the start and around each read:

- the VIN, required again even when step 2 matched it. A well-formed VIN equal to the journal's
  target VIN promotes the guards to the per-vehicle lock again (`JobGuards::take_vehicle`
  returns at once when they hold that vehicle, so step 2's promotion is not repeated in
  effect). When step 2 could not read the VIN, this promotion is the fallback that takes the
  lock, after the passive teardown. A well-formed VIN that differs ends the job in
  `JobError::IdentityMismatch { identity: Vin }`. Anything else (no target VIN or no declared
  source, no answer, a negative response, a value that is not a well-formed VIN, a worker
  failure) ends it in `OnSiteInterventionRequired(IdentityNotEstablished { flash_session,
  identity: Vin, teardown })`, where `teardown` is step 2b-1's outcome, so the technician
  knows whether an ECUReset was sent;
- the hardware identity, only after the VIN matched: the raw field bytes must equal the
  journal's. Different bytes end the job in `IdentityMismatch { identity: HardwarePartNumber }`,
  and anything else (no recorded value or source, no answer, a negative response, a worker
  failure) in `IdentityNotEstablished { identity: HardwarePartNumber, teardown }`.

The reads put no VIN in a log message or a result.

`restart::check_state` is step 3b-2 (ADR-229 item 2 step 3, ADR-268 item 5), run after step 3a.
It reads the software version through `identity.software_version` as raw field bytes, at most
`1 + version_read_retries` times (the plan's value), pausing for the job's poll interval between an
inconclusive read and the next (none before the first read or after the last; the pause is
cancel-checked and sends nothing), and checks for a cancel at the start and around each read. The bytes are compared with the journal's facts, in this
order:

| Read | Journal | Result |
|---|---|---|
| a version equal to the intended one | post-transfer steps complete for the interrupted pass | `StateCheck::ReadBackVerification` |
| a version equal to the intended one | not complete for the interrupted pass | `StateCheck::RedoTransfer` |
| a version equal to the pre-erase one (and not the intended one) | any | `StateCheck::RedoTransfer` |
| any other decoded version | any | `OnSiteInterventionRequired(UnexpectedSoftwareVersion)`, conclusive, not retried |
| a negative response with the plan's declared `no_application` code | any | `StateCheck::RedoTransfer`, conclusive |
| no answer, another negative response, a field that does not decode, a worker failure | any | read again; after the last read `OnSiteInterventionRequired(SoftwareVersionNotEstablished)` |

"Complete for the interrupted pass" means the completion is journaled and the interruption point
is no later than the completed pass's last post-transfer step (the test the classifier applies): a
program that stepped back into the plan's entry after the completion and crashed in the later pass
is redone, not read back (ADR-269).

A program that declares no software-version source ends in `SoftwareVersionNotEstablished`
without a read. When the intended version equals the pre-erase one, only the journal's record of
the completed post-transfer steps tells a finished job from an unstarted one. A job that names no
intended version never reaches `ReadBackVerification`: the pre-erase version is redone and any
other decoded one is unexpected. Both reasons carry the step 2b-1 teardown. The step sends only
ReadDataByIdentifier requests, so nothing changes the ECU, and a completed path sends no
ECUReset in the whole restart.

`restart::check_reentry` is step 4a (ADR-229 item 2 step 4, design 8.9), run only when the state
check decided `StateCheck::RedoTransfer` and before anything of step 4 is replayed, since the
conditions may have changed while the agent was down. It checks every declared precondition in
the order voltage, external supply, ignition, engine, vehicle speed, and stops at the first that
does not hold. The ECU was confirmed in its default session, so each is read through its
`default_session` source only, never through the `programming_session` one (that source is for
step 4b-3's second check before the erase, `restart::check_before_erase` below); the gates of step 2, which do not know the ECU's
session, fall back to it, and step 4a does not. A value outside the declared range, a reading
that is not a value, a source the table does not map and a worker failure end the job in
`OnSiteInterventionRequired(PreconditionNotMet { flash_session, precondition, teardown })`, with
the first failed precondition and the step 2b-1 teardown. The software-version match of design
8.9.1 is not checked here, since step 3b-2's rules replace it during a restart, and neither are
the VIN and the hardware identity, which step 3a established. A cancel stops it at the start and
around each read. It sends only ReadDataByIdentifier requests through declared sources and reads
runtime inputs. A restart that goes to the read-back verification does not run it. Each read is
made once, as in the gates and step 3a; only step 3b-2's version read is retried. `WorkerHost`
reports every runtime input but the supply voltage as not established (the `inputs` module), so
with it a program that declares the external supply, ignition, engine or vehicle speed through a
runtime input ends a redone transfer's restart in `PreconditionNotMet`.

Replay to the erase (step 4b-2, ADR-273): after step 4a passed, the runner runs the program from
`RestartPoint::entry_state` (`Vm::resume`), with the journaling of a first run, up to the plan's
`erase_pc`, and stops when execution arrives there, before the journal's `arrive` and before the
instruction. Steps before the entry are not run again; the steps from the entry to the erase
(programming session, security access, setup routines) are sent in order, and nothing at or after
the erase, so no transfer-start marker is committed. The replay commits no run start: its starting
state is already the journal's newest, so a crash during it restarts from the same entry state.
It shares the step loop with a run from instruction 0 (`run_vm`) and ends as a first run does on a
failed step, a wait nobody answers, the step limit or a cancel.

`restart::check_before_erase` is step 4b-3 (ADR-229 item 2 step 4, ADR-245 item 6), run when the
replay stopped at the erase. The ECU is in its programming session then, so it checks every
declared precondition (supply voltage, external supply, ignition, engine, vehicle speed; the VIN
and the hardware identity are not preconditions and are not read) in the order of step 4a and
stops at the first that does not hold, reading each through its `programming_session` source only.
A missing source (which `Program::validate` refuses for a restartable plan), a value outside the
declared range, a reading that is not a value, a source the table does not map and a worker
failure all mean not met. Each read is made once; a cancel stops it at the start and around each
read. A failure ends the job in `OnSiteInterventionRequired(PreconditionNotMetBeforeErase {
flash_session, precondition, teardown })`, a reason apart from step 4a's `PreconditionNotMet`: it
tells the technician that the ECU went through the replay (it is in its programming session with
the setup steps run) and that no erase was sent. `teardown` is the step 2b-1 outcome, from before
the replay.

A job that passes ends in `RestartOrderUnavailable { flash_session, teardown, confirmed, state }`
with `state` the `StateCheck` found: the erase (step 4c) and the read-back verification do not run
in the agent yet.

## Job guards

The `guards` module holds a job's exclusive locks on the device (design 8.8, 8.8.1; ADR-256,
ADR-257, ADR-262): the per-VCI lock, the device's single reprogramming slot and the per-vehicle
lock. Each is an OS lock (`File::try_lock`) on a file in the device's lock directory: `vci-{hex
name}.lock`, `reprogramming.lock` and `vehicle-{k:03x}.lock`. The OS releases a lock when its
process dies, so a crashed run never blocks the next one, and the files are never deleted.

- `JobGuards::take(GuardSetup { dir, vci }, poll, cancelled)` takes the per-VCI lock, then the
  slot, for a job that writes (`policy::writes`). It waits while another job holds either,
  retrying every `poll`, and stops on a cancel (`GuardError::Cancelled`).
- `JobGuards::take_vci_only(...)` takes the per-VCI lock alone, for a job that only reads, so
  reads on other VCIs go on while a job reprograms. `JobGuards::holds_slot` tells the two
  apart.
- `JobGuards::take_vehicle(vin, poll, cancelled)` takes the per-vehicle lock, on guards with or
  without the slot, for a well-formed VIN only (`GuardError::InvalidVin`). The file is one of
  4096 buckets: `k` is the first two bytes of SHA-256 over the VIN, read big-endian, masked to
  their low 12 bits (a digest starting `84 b1` gives `vehicle-4b1.lock`). Before it opens its
  bucket, the call lists the lock directory and, when fewer than 4096 bucket files are present,
  creates every missing one in bucket order (on Unix each made readable by everyone whatever
  the umask before it is hard-linked to its name, then a best-effort directory sync whose
  failure is only logged), so whether files
  are created depends only on the directory's state, never on the VIN, and neither a file's name
  nor the directory's content reveals a VIN (ADR-262). Two vehicles in one bucket exclude each
  other, which delays a job about once in 4096 concurrent pairs. Taking the vehicle the guards
  already hold returns at once; another VIN is refused (`GuardError::OtherVehicleHeld`), as are
  guards marked `link_unconfirmed`. `JobGuards::holds_vehicle` tells whether they hold one.
- Every lock file must be a regular file. It is opened without following a symbolic link and
  without blocking, and a symbolic link, FIFO, device or (on Windows) reparse point at its path
  fails the take (`GuardError::NotAFile`, or `GuardError::NotAVehicleFile` for a bucket file,
  which names no path so that no bucket appears in an error, or an I/O error) instead of being
  followed or hanging (ADR-262 item 6).
- Locks are always taken in the order VCI, then slot, then vehicle, and held until the
  `JobGuards` is dropped.

Every entry point (`run_program`, `run_program_journaled`, `resume_program_journaled`) takes
the guards by value and returns them with the run's result, so two runs can never use one set
at once. A program that writes and that this build allows (a debug build, on the simulator),
run on guards without the slot, ends in `JobError::NoReprogrammingSlot` before anything opens;
a release build refuses every writing program first (`JobError::Refused`). A new run after an agent crash or a loss of the
device's power takes them with `JobGuards::take` before it calls the runner, so a duplicate
resume of the same job waits there without opening a link. A dropped future releases them only
once the job thread has ended. A job that survives a worker crash, a VCI disconnect or a loss
of the vehicle's supply alone gets them back, still held, and passes them to its next run. A
VCI name has 1 to `MAX_VCI_NAME` (100) bytes. The lock directory is one per device,
shared by every agent process on it whichever user it runs as: each of them must be able to
create files in it, list it (the per-vehicle lock counts its bucket files, ADR-262) and read the
lock files there (lock files are opened read-only).

A run whose link close fails or panics, or whose open fails partway and cannot close what it had
opened, gives its guards back marked (`JobGuards::link_unconfirmed`), whatever the job's own
result (ADR-258). Marked guards refuse another run (`JobError::LinkUnconfirmed`), and dropping
them keeps their locks until the process exits. The caller stops the worker that held the link
(`WorkerProcess::stop`, which returns `Ok` only once the child is reaped) and then calls
`JobGuards::worker_gone`, before the guards serve another run or are released. The runner closes
the link exactly once on every path after it opened, a panic included. An agent killed without
running its exit path still frees its locks while its orphaned worker tears the link down on
stdin EOF; the next open of a device still held usually fails. On Unix the guards refuse a
directory that group or others may write unless it has the sticky bit and those users may also
read it (`GuardError::UnsafeDir`, or `GuardError::UnlistableDir` for a sticky directory they
cannot read): another user could otherwise delete a lock file a job holds, and a second job
would lock the new file at the same path. A directory the guards create is writable by its owner
only. `ngr-agent run` takes the guards from `--locks` and `--vci` before it runs the program,
and holds them until the run ends.

## Restart inputs

The `inputs` module supplies what an interrupted-write restart reads (design 8.2.5, 8.9;
ADR-229, ADR-245 item 2). `resolve_source` reads one `diag_ir::Source` and gives a `Reading`:
an integer, a text, `CannotBeEstablished` or `CannotBeDecoded`. The last two are readings, not
errors; a check treats them as failed. A failure to reach or use the worker (a transport
failure, a refused RPC or a request the policy refuses) stays a `HostError`, which fails the
check as well.

- Runtime inputs (`RuntimeInputs`, implemented by `WorkerHost`, which is also the sender, so
  `resolve_source(source, &table, &mut host)` takes one value): the supply voltage is read
  through the worker's IoCtl RPC. The host looks up the id of `PDU_IOCTL_READ_VBATT` by name
  (`GetObjectId`, IOCTL object type) on first use and keeps it. When the J2534 worker does not
  know the name, the VCI offers no voltage and the input reads "cannot be established"; a
  D-PDU worker reports an unknown name as an internal error, which stays a `HostError`.
  The worker interface has no defined source for external supply, ignition, engine running or
  vehicle speed: raw pin voltages and analog inputs carry no agreed meaning for them, and
  `sim-vci` simulates only the battery voltage (ADR-238). So they read "cannot be established". `FixedInputs` holds fixed readings for tests.
- Service fields: `ServiceSources` is a table from `(service_id, field_id)` to the request
  (SID first), the offset after the response SID, the length and the encoding (unsigned
  big-endian integer, or printable ASCII). It is a stand-in until the declaration part has a
  decoder (ADR-245, consequences). Until then a table accepts only single-identifier
  ReadDataByIdentifier entries (request `[0x22, hi, lo]`, offset 2): other services do not echo
  their request in a form the table can check, and a request for several identifiers lets the
  ECU leave some out. Building a table also refuses a duplicate id pair. The response must be
  exactly the positive SID, the echoed identifier and the field. A source missing from the
  table, a negative, short or long response, a wrong echo, or a field that does not fit its
  encoding reads as "cannot be decoded". ASCII text keeps surrounding spaces as they are; a
  field of only spaces, or with any byte outside 0x20 to 0x7E, cannot be decoded.
