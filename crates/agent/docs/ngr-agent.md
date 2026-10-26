# `ngr-agent`

`ngr-agent` is the binary of the `agent` crate (design 3.3). It has one command, which runs the
procedure of a variant's document on a VCI for local end-to-end runs and tests; it does not
connect to the server or take jobs from it. A job is always a document: a bare program has no
declarations to decode with and no declared link, so the command takes none.

## `ngr-agent run`

```sh
ngr-agent run --vci <name> --mode <device|user> --document <file> [--artifact <file> --artifact-sha256 <hex>] [--api <j2534|dpdu>] [--workers <dir>] [--locks <dir>] [--comm-log <dir>]
```

| Option | Meaning |
|---|---|
| `--vci` | The library name, resolved the same way by the agent (step 3) and the worker service (design 7.1.1). For J2534 v04.04: the registry key name on Windows, the `Name` of a registration definition on Linux. For the D-PDU API: the `SHORT_NAME` of an implementation in the root description file |
| `--api` | The API the VCI's library implements: `j2534` (J2534 v04.04, `j2534-0404-service`; the default) or `dpdu` (the D-PDU API, `iso22900-service`). The names are those of a VCI profile's `standard.kind` (`schemas/vci-profile.schema.json`) |
| `--mode` | The device's operating mode (design 6.2), `device` or `user`. Required: the mode is fixed per device at installation, and this command, which has no installation of its own, takes it from the command line rather than defaulting to either. It is passed to the worker (ADR-281 Decision item 2); design 7.2 gives it its meaning for a failed library check: device mode refuses the library, user mode reports the findings and loads it. Both worker services apply it to the checks they run before loading |
| `--document` | A variant's document (design 8.2.6, `.dir`): its procedure is the program, and the job decodes response fields through its variant's declarations (`agent::JobDocument`). The document is checked as `diag_ir::declaration::load_ecu_document` checks it (ADR-296), and one without a procedure, or whose procedure is not exactly one program, is refused (ADR-318). Required, given once. The options `--program`, `--tx-id` and `--rx-id` of earlier versions are gone: they are refused as unknown options (exit status 2) |
| `--artifact`, `--artifact-sha256` | The flash artifact a procedure reads its blocks from (`Op::FlashBlock`, design 8.2.5, 11.3, ADR-336 items 1 and 12), and the SHA-256 the job names for it, 64 hexadecimal digits in either case. Both or neither, each at most once. The file is read once, before any worker starts, and its bytes stay in memory for the job (`agent::Artifact`), so a file changed on disk after the check cannot change a block. See step 1 for the check |
| `--workers` | Directory of worker builds, laid out as `<dir>/<ABI name>/j2534-0404-service[.exe]` and `<dir>/<ABI name>/iso22900-service[.exe]` (design 7.3). Default: `workers` next to the `ngr-agent` executable |
| `--locks` | The device's lock directory for the job's guards ("Job guards" below), as an absolute path. Default: `locks` next to the `ngr-agent` executable. Lock files are opened read-only, so a file another user created still locks. The agent creates a missing directory writable by its owner only, so a device whose agents run as several users needs it prepared beforehand: writable by all of them, and with the sticky bit (or an equivalent ACL) so that no user can delete another's lock files (ADR-257). It must also be readable by every agent user, since the per-vehicle lock lists it, and on Unix support hard links, with which it creates its files (ADR-262). On Unix a directory that group or others may write without the sticky bit, or without being readable by them, is refused |
| `--comm-log` | The directory of the run's communication log (design 8.11, ADR-347), created if missing (on Unix writable and readable by its owner only; an existing directory keeps its permissions, which are not checked); at most once. The run gets a job ID of its own (`run-{unix ms}-{process ID}-{count}`, generation 0), so two runs never share a log, and the log's path is printed on stderr after the run, when the log was opened. A log that ends incomplete is reported on stderr and does not change the exit status. Without it the run writes no log, unless its procedure logs with `diag.log`: then it gets a log of its own in `commlog/` beside `ngr-agent`, as `--locks` defaults to `locks/`, with its path and an incomplete end reported the same way ("Communication log" below) |

The job opens the link its variant declares (`agent::Job::declared_link`, ADR-324):

- The variant must declare exactly one protocol. It must carry `UDS`, under the short name
  `ISO15765` or `ISO_14229_3_on_ISO_15765_2`, without extended addressing.
- The baud rate and the two identifiers are the protocol's.
- `CP_P2Max`, `CP_P2Star` and `CP_RC78CompletionTimeout`, in microseconds, give the timings,
  rounded up to whole milliseconds. Without them the timings are the link's defaults
  (`LinkConfig::iso15765`): P2 1 s, P2* 5 s and a 25 s bound on a response-pending chain.
- The protocol's other ComParams are set on the link as declared, after the link's own, with
  the class the worker takes from the ComParam. Names compare ignoring case.
- Refused: a ComParam the link sets itself, one that lets a primitive outlast the host's
  deadline (`CP_RC21*`, `CP_RC23*`, `CP_RepeatReqCount*`, `CP_ModifyTiming`), a name not
  starting `CP_`, a ComParam declared twice or with a unit, and a value outside an unsigned
  32-bit integer (ADR-324 item 3).
- A document whose link cannot be used fails before any worker starts. A ComParam the worker
  does not know, or resolves to one the link already set, fails when the link is set up,
  naming the ComParam.

The link's resource is named by the ISO 22900-2 (2022)
short names of UDS on dual-wire CAN (Annex B.1, B.2), which both worker kinds resolve
(`agent::link::resource_data`, ADR-319).

Steps (`agent::check_job`, `agent::guards::JobGuards`, `agent::launch::launch_worker`, then `agent::run_program`):

1. Read the document and its procedure, and check the job (`agent::check_job`):
   a program that decodes a field the document
   does not declare (ADR-318), a generic send-receive (`Op::SendReceive`, ADR-321) whose
   service the document does not declare with a request and a positive response, or whose
   variant's negative-response shape the workers do not support (ADR-322), a program whose schema version or
   size this VM does not accept, one whose restart declaration `Program::validate` refuses
   (ADR-245), one with a request this build may never send (a release build sends only
   the read-only requests of the variant's application layer; ADR-235 item 8, ADR-247,
   ADR-342), or one with a `ReadDtc`, which no build serves (ADR-342), fails here,
   before any worker starts. The same check refuses a job whose artifact does not hash to the name it gives (also when no instruction reads it), a program with a `FlashBlock` and no artifact, and any segment of a flash session the program names (by a `FlashBlock` or a flash recovery plan; artifact offset plus length, overflow-checked) that reaches past the end of the file (`JobError::ArtifactMismatch`, `NoArtifact`, `SegmentOutsideArtifact`). A bad operand, such as a missing constant, fails only when the VM
   reaches it, after the worker has started.
2. Take the job's guards from the `--locks` directory, named by `--vci` ("Job guards" below):
   the per-VCI lock, plus the reprogramming slot when the program writes (`Job::writes`; a
   send-receive counts by its declared service, ADR-322).
   The run waits here while another job on the device holds them.
3. Resolve the VCI name the way each worker build resolves it on its side (design 7.1.1). For
   J2534, through `ngr_library_resolver::j2534_0404`: the build's registry view on Windows, the
   registration definition on Linux; in a debug build the `NGR_J2534_DEFINITION_DIR` definition
   directory replaces both. For the D-PDU API, through `ngr_library_resolver::iso22900`: the
   root description file that the build's registry view names on Windows, the fixed root file on
   Linux; in a debug build the `NGR_PDU_API_ROOT_FILE` file replaces both (ADR-282). No
   `config.toml` key names the library (ADR-281 Decision item 1). The first build whose library header (`worker_host::abi::detect_file`) matches the
   build's own ABI is chosen, so the header that selects the build belongs to the file that
   build loads (ADR-240).
   The same builds are tried in the same order for both APIs:
   - Windows x64 agent: the x64 build (64-bit registry view) first, then the x86 build (32-bit
     view, `KEY_WOW64_32KEY`). A VCI registered in both views runs on the x64 build.
   - Windows x86 agent: the x86 build only, so a 64-bit library is not found.
   - Linux: one lookup; the build follows the header's ABI.
4. Pick the API's service build for that ABI with `WorkerLayout::find`. A missing build fails
   as `UNSUPPORTED_ABI`.
5. Launch the service with the library name and the `--mode` value, and a J2534 service also
   with its `long_size`: the resolved registration definition's `LongSize` (design 7.1.1), else
   the ABI's default (7.1.2; ADR-284). An 8-byte `LongSize` for an ABI whose `unsigned long` is
   32 bits wide fails the run before the service starts. Then provision its auth key and connect
   the gRPC client (design 7.4). The job's link set-up (step 6) names its resource by protocol,
   bus type and DLC pins, which both worker kinds resolve (ADR-319), and sets the link's
   ComParams by their ISO 22900-2 (2022) short names (Annex B.3). A run with `--api dpdu` is
   tested end to end on `iso22900-mock` with a simulated ECU attached (ADR-299,
   `crates/iso22900-service/tests/ngr_agent_dpdu_end_to_end.rs`).
6. Run the program under the request policy and the default job limits (ADR-235), then stop
   the worker and release the guards. The stop request may take up to 2 s to be answered; a
   worker that has not exited 5 s after that is killed and reported on stderr. When the job's link was not confirmed closed and the worker
   cannot be stopped either, the run fails with "the worker may still hold the VCI", and the
   locks stay held until the process exits (ADR-258). The policy is read-only, except that a debug build may send any request once
   the worker's VCI has identified itself as `sim-vci` (ADR-247) or `iso22900-mock` (ADR-331); a program that needs more
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
The positive RequestDownload response also sets the transfer's limit: its
maxNumberOfBlockLength (clause 14.2.3, the length its lengthFormatIdentifier's high nibble
names), which counts the whole TransferData request, SID and counter included. A block whose
`2 + data.len()` exceeds it fails as `HostError::BlockTooLong` before it is sent, and the
tracked transfer stays as it was (ADR-336 item 12). A positive response with no readable limit
(a length of 0, a reserved low nibble that is not 0, or not exactly the length it names) still
begins the transfer, which the ECU has accepted, but its first block fails as
`HostError::NoBlockLength` before it is sent; a limit wider than 64 bits is never reached.
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
| 1 | Reading the document, launching the worker or the job failed (including a procedure that ended itself as failed, `JobError::ProcedureFailed`), the worker may still hold the VCI after a failed link close, or the final state holds a non-finite float (infinity or NaN), which JSON cannot represent | The reason on stderr |
| 2 | Invalid command line | The error and the usage on stderr |

The worker's own log goes to stderr.

## Operational log

The agent logs its own `tracing` events (design 12.2, ADR-348), with the settings of its own
table in the configuration file, `[config.agent.logging]` (`level`, `output`, `dir`;
`crates/vci-service-config/docs/logging-config.md`). Without them it logs at level `info` to two
places; `output = "null"` turns both off, the only setting that turns the file off (ADR-348
item 3).

- **stderr**, as one line of text per entry. When stderr is not a
  terminal, for example when a service manager collects it, each `external` field is written as
  its length only, since the OS may forward that output off the device (ADR-348 item 6).
- **A JSON Lines file** (`ngr_oplog`) in the configured `dir`, else the user's state directory:
  - `$XDG_STATE_HOME/nekoguruma/logs/` (an absolute `XDG_STATE_HOME`), else
    `~/.local/state/nekoguruma/logs/`, on Unix;
  - `%LOCALAPPDATA%\nekoguruma\logs\` on Windows.

  Each running agent writes a file of its own, `agent.{n}.log`, where `n` is the smallest
  number whose `agent.{n}.lock` it can lock. The file is rotated at 10 MiB, and 5 rotated
  files are kept. When the file cannot be opened, the agent warns on stderr and logs there alone;
  that stderr is the fallback ADR-348 item 5 treats as forwardable, so it writes `external`
  fields as their length only even on a terminal.

The agent checks the configuration file with the design 7.2 writability rules before it reads
the settings, as a worker does (ADR-285), with the device's `--mode`: in device mode a file
regular users can change is not read, and the agent logs with the defaults above and warns; in
user mode the file is read and the findings are logged as a warning. A setting that cannot be
used (a level that is not a filter directive, an output other than `"stderr"` or `"null"`, a
relative directory) keeps its default, with a warning, and so do all of them when the file does
not parse. The paths and the file's own text in those warnings are in their `external` field.

A run's entries are in a `job` span with its job id (the communication log's, when there is
one), generation 0 and the VCI name in the span's `external` field. The span is kept at any
configured level, so an entry logged during a job carries those fields even when the level is
above `info`. What an entry may hold is
`operational-log.md`.

## Configuration file

The agent and the worker find the library without `config.toml`, from the registry on Windows
and the registration definition directory on Linux (design 7.1.1), so the configuration root
(`vci-service-config`'s `config-root-*` features) does not change which library either one
resolves. The definition directory is fixed at build time (`NGR_J2534_DEFINITION_DIR` when
building, ADR-228); build the agent and the workers with the same one, or the agent can detect
the ABI, and pass the `LongSize`, of a different library than the worker loads. The worker's per-library settings still
come from its own `config.toml`, which with `config-root-exe-dir` is the file next to its build
under `--workers`.

## Write-job journal

The `agent` library's `journal` module keeps the write-job journal (design 5.5, ADR-244): one
append-only file, `{job_id}.g{generation}.journal`, per job and ownership generation, in a
directory the caller passes. Every commit is synced before it returns. Only the job's writer opens
the journal for writing: `Journal::create` and `Journal::open` take an exclusive OS lock on the
sidecar `{job_id}.g{generation}.journal.lock` and hold it while the journal is open, so a second
writer gets `JournalError::InUse` (ADR-255). Dropping the `Journal` unlocks the sidecar before
closing it, so a child process spawned meanwhile, which shares the open lock file until it runs its
program, does not keep the journal locked. The lock goes with the process, so a crashed run never
blocks the next one. Other readers use `Journal::read`, which takes no lock and never
changes the file. The journal only records; committing an intent marker before the request it guards, and stopping the job when a
commit fails, are the job runner's duties.

`agent::run_program_journaled` is the runner that keeps it (ADR-252). It takes a
`JournalSetup` (directory, job key, the `ServiceSources` table for the ECU's identity, which
only a job without a document uses (below), and the VIN the job targets, which a first run records as the journal's first record and a
restart compares against, and the software version the job intends to write, recorded right
after it in the same creating write and compared the same way, ADR-268) and the job's guards ("Job guards"
below), which it returns with the result, and,
for every program (ADR-335 item 1; before it, only for one with a flash recovery plan or a section
marked `RecoveryRequired`, ADR-293), creates the journal once the link is open and the policy
allows the program, before anything is sent to the ECU. A journal that already exists ends the job with nothing sent: a job that ran before goes
on through `resume_program_journaled` ("Restart entry" below). Every run from instruction 0, a first
run or a plain start on an existing journal, commits the VM state it starts from as its first
record (the run start, ADR-272), at the run's first step count and before anything is sent, so
the journal marks where each such run began. The restart's replay commits one too, carrying the
entry state it starts from (ADR-274; "Replay to the erase" below); the continuation after it
commits none. Each time execution arrives at a plan's boundary, before that
instruction runs, it commits:

- at the entry, once per job and before its first transfer: the hardware part number and
  software version, read as raw field bytes (`inputs::read_field_bytes`) through the sources
  the program declares. A software version answered with the plan's declared "no valid
  application" response is not recorded, and the job goes on;
- at the erase: the transfer-start marker;
- at RequestTransferExit: its marker;
- at the first diagnostic primitive at or past a plan's recovery-required point that is neither
  of those: a request intent, so a crash before its response still places the interruption
  there (ADR-253);
- at the first diagnostic primitive of a pass through a section marked `RecoveryRequired`
  outside every plan's range: a request intent (ADR-291). A pass ends when execution arrives at
  an instruction outside such a section;
- at every diagnostic primitive outside every plan's range (`entry_pc` up to, not including,
  `post_transfer_end_pc`) that is not safe to repeat: a request marker (the `Request` record,
  ADR-335 item 2). A primitive is safe to repeat when at least one section covers it and every
  section covering it is `Safe` (`restart::safe_to_repeat`); one outside every section, or under a
  `Safe` and a non-`Safe` section at once, is not. The runner takes the cancel check an intent
  has before the commit, so a cancel that arrives while the marker is being committed still
  leaves a marker for a request that was never sent, as an intent can.

When an instruction completes, it commits the block of a `FlashTransfer`, under the host's
running index (`TransferProgress`, ADR-250); a step record for every diagnostic primitive inside
a plan's range; the VM state on the record of a step that brings execution to a plan's entry
(a job that starts at the entry has its run start instead); a checkpoint, the VM state after the
step, for every diagnostic primitive outside every plan's range, except where the step already
carries a boundary state at the same step count (the `Checkpoint` record, ADR-335 item 2); the VM
state on the record of a step from inside the plan that brings execution to its post-transfer end, whatever its
instruction (one record when that pc is also the next plan's entry; a jump to the end from
outside the plan, such as a polling loop after it, records nothing; ADR-271 item 2, ADR-274); and the post-transfer completion
with that same step, right after the state. A commit that
fails, or a declared identity source that gives no value (`JobError::IdentityUnreadable`, or
`JobError::IdentityRead` when the read cannot be sent), ends the job before the next
instruction runs, so a guarded request is never sent without its marker and nothing is erased
without the identity.

When the procedure ends itself as failed (`Op::Fail`, ADR-280), the runner commits a
procedure-failure record before it returns `JobError::ProcedureFailed { pc, code, detail }`: the
`Fail` instruction's position and the procedure's code and detail, the detail decoded lossily and
the code and detail cut to 128 and 1024 bytes (`Program::validate` already keeps the code within
128 bytes and non-empty). It is the journal's last record: it must come
after the last step, and the journal refuses every record after it, in a commit and when it reads
the file back (`RecoveryFacts::procedure_failed`). A job without a journal (`ngr-agent run`, or a
plain start of a missing journal, "Restart classification" below) ends in the same error with
nothing recorded.

A run that stops on the step limit (`JobError::StepLimit`) or on an instruction the VM refuses
(`JobError::Vm`) commits a run-stop record the same way before it returns its error (ADR-288): the
instruction it stopped at, the step count it had reached and the reason (`RunStop::StepLimit` or
`RunStop::Vm` with the VM's error). It is terminal like the procedure-failure record
(`RecoveryFacts::run_stopped`): it must come after the last step, nothing may follow it, and a
commit that fails ends the job in `JobError::Journal` instead. A VM error found before the run
(the program's or the journaled state's schema version) is not a run's stop and is not recorded.

A restart that ends in `JobError::IdentityMismatch { identity }` (the gates of step 2 or step 3a)
commits an identity-abort record first (`Journal::commit_identity_mismatch`, ADR-286): the
`IdentityKind` only (a VIN or a hardware part number; the journal refuses a software version), with
no position, VIN or value. It is terminal like the procedure-failure record: the journal refuses
every record after it and after a procedure failure, in a commit and when it reads the file back.
`RecoveryFacts::identity_mismatch` and `RecoveryFacts::procedure_failed` hold the two endings. A commit that fails ends the job in `JobError::Journal`, and the mismatch is only
logged. No on-site ending is journaled.

`ngr-agent run` uses `run_program` and keeps no journal; it sends read-only requests, or any
request to `sim-vci` or `iso22900-mock` in a debug build (ADR-247, ADR-331).

A plain procedure, the steps outside every plan, is journaled by two records appended to the
format, with no version change (ADR-335 items 1 and 2; `Journal::commit_checkpoint` and
`Journal::commit_request`). Both carry the position of the primitive (its pc and the step count
before it runs, as a step record does), so a checkpoint and the step record of the same primitive
share one step count:

| Record | Committed | Folds into |
|---|---|---|
| `Checkpoint { at, vm_state }` | after a diagnostic primitive outside every plan's range, with the VM state after it; not where the step carries a boundary state | `JournalState::latest_checkpoint` only |
| `Request { at }` | before a diagnostic primitive outside the plans that is not safe to repeat | `RecoveryFacts::first_request` and `last_request` |

The checkpoint must come after the last step and not before the last request or the latest
checkpoint (it may share their step count), and a request after the last step and after the
previous request. A VM state whose record would not fit in a frame (`MAX_FRAME`) leaves no
checkpoint and does not end the job: the commit is refused before anything is appended, the
journal is unchanged, and the run goes on. A missing checkpoint only makes a restart more
conservative (ADR-335): the latest checkpoint stays an older one, and a completed request that is
not safe to repeat with no state after it counts as unconfirmed. `JournalState::latest_checkpoint` is the newest state of any record that carries
one: a checkpoint, a run start or a step with a state (a run start is at the step count it starts
at, the others at the step they completed). The facts a flash plan's restart reads (the last step,
the last and previous VM state, the last intent, the interruption point) are not changed by either
record, so the classification of a journal with a plan is the one described below.
`restart::next_steps` counts them, so a run that goes on with the journal records after them. A
primitive costs one fsync for its checkpoint and two when it is not safe to repeat. The restart
classification reads the new facts ("A plain procedure" below).

Because every journaled job now keeps a journal, a plain program's declared failure, step-limit
stop or VM refusal is terminal across resumes, as for a plan program (ADR-280, ADR-288), and
`run_program_journaled` called again with an existing job key on a plain program fails because
the journal exists, instead of running the program again.

The restart's use of the records is the classification of a plain procedure, below.

## Communication log

The communication log (design 8.11, ADR-346) records a job's exchanges for a reader who must
tell a vehicle fault from a fault of the diagnostic system. `agent::comm_log` writes it
(ADR-347):

- **File.** `{job id}.g{generation}.commlog.jsonl`, one JSON object per line, beside the job's
  journal, under a writer lock of its own (`{job id}.g{generation}.commlog.lock`). Every run of
  the job and generation appends to the same file. The journaled entry points log under their
  journal's directory and key; `run_program` logs when the job carries a setup
  (`Job::with_comm_log`), and a journaled job whose setup names another directory or key ends in
  `JobError::CommLogMismatch` before anything opens. A procedure that logs with `diag.log` always
  has a log, since its messages go nowhere else (ADR-348 item 6): a `run_program` job without a
  setup gets one of its own (`CommLogSetup::for_run`) in the agent's communication-log directory
  (`Job::with_comm_log_dir`), and without that directory it ends in `JobError::NoCommLogDir`
  before anything opens.
- **Lines.** Every line has the next `seq` and an `event`. A new log starts with the `header`
  (seq 0: the job's ID, generation, kind and document SHA-256, the target VIN when the job names
  one, the ECU variant, the agent and IR schema versions, and a reading of the agent's clocks).
  Each run writes `job_start` (the agent's version, `run`: `first`, `resume`, or `plain_start`
  for a resume that finds no journal, and whether the job writes) and, when it ends,
  `job_end` with the outcome (`ok` or the `JobError` variant's name). A reopen writes
  `agent_clock` first, a new reading that starts a new agent epoch. Right after the link opens,
  before anything is sent on it, the run writes `vci_connected`, then `link_opened`.
  `vci_connected` names the API (`dpdu` or `j2534_0404`) when the job was told it
  (`Job::with_vci`, which `ngr-agent run` sets from the worker it launched), the `unsigned long`
  width of a J2534 worker, and the module's version data, which the link reads with
  `GetVersion` in every build (`null` when the worker gives none):
  - a D-PDU module's vendor, hardware name, serial number and version, firmware name and
    version, and API library name and version;
  - a J2534 worker's firmware, library and API versions only, which is all v04.04 gives.

  Its `drift_bound` and `clock_margin` have `source` `none`, since neither is configured yet.
  `link_opened` then gives the link, its endpoint and
  channel, the protocol, baud rate and identifiers, and every ComParam by name with its value
  and `set_by` (`link` for those `agent::link` sets to its own value, `document` for those
  whose value the document gives, declared or the baud rate and a declared timing,
  `library_default` without a value for the timing ComParams design 8.11 lists that neither
  sets). The link sets `CP_RC21Handling` and `CP_RC23Handling` to 0, so the API repeats no
  request on a busy answer, and turns `CP_TransmitIndEnable` and `CP_StartMsgIndEnable` on;
  their entries carry `accepted`, and a library that refuses one opens the link without it.
  `transmit_confirmation` names `transmit_indication` when the library accepted
  `CP_TransmitIndEnable` (the link turns no loopback on). The event gives response times not
  judged and no timing margin (`source` `none`), since no indication is used as evidence yet.
  `forwards_pendings` is what the worker answered to `CreateComLogicalLink`
  (`Link::forwards_pendings`, ADR-355): `true` for the J2534 worker, `false` for the D-PDU
  worker, `null` from a worker that does not say. The agent takes it from that answer alone, and
  each exchange on the link has `pending_visibility` `forwarded` only when it is `true`, since
  the link has the API handle 0x78 alone (`send_receive`'s supported pending codes); otherwise
  `absorbed_by_vci`. The host keeps each pending bound to the primitive for the exchange's
  `pending` list, with its VCI time and clock as the worker gave them and the agent's time when
  the host read it (not yet its interval from the transmit or the previous pending), so a
  forwarded link's empty list means no pending came.
  The worker host reads every item of the link while a primitive runs and hands the result and
  error items of no primitive (a transmit indication, a link error) or of an earlier primitive
  to the log with the exchange during which it read them, for an exchange it logs
  (`CommLog::exchange_with`). A transmit indication that carries the exchange's request
  identifier (from the item's header) is the exchange's send evidence (`sent_evidence`
  `tx_indication`), unless an ended exchange on the link could still own it: one with `sent`
  `unknown`, which a late TxDone may belong to (design 8.11 `sent_evidence`, ADR-346 item 4).
  Design 8.11's other conditions hold on the agent's link by construction: one primitive runs at
  a time on the job's one link, and the link starts no communication, so the API sends no
  tester present of its own (ISO 22900-2:2022 8.2.6.3.2 and 8.4.9.2.1 run it only on a link
  whose communication was started). A start-of-message indication that carries the exchange's
  response identifier is the first frame of its response, which counts as an answer of the ECU
  (design 8.11 "Link settings for the log", "Answer of the ECU"), unless an owed request on the
  link expecting a response on that identifier could own it (one that stays owed after a reopened
  link too, so it only keeps the indication `unattached`). The exchange then has `sent`
  `confirmed` with `sent_evidence` `answer` (a transmit indication still wins), and counts as
  answered for the later exchanges. A start with no final response or pending is no matched
  answer and leaves H2 open, in this exchange and the later ones, since the worker may have
  discarded the full response (`Evidence::bound_answer`, design 8.11 "Matched answer",
  ADR-354); the rows of a pending seen do not take it either, while every other row still
  counts it as an answer of the ECU. Every
  start with that identifier read during the exchange attaches, since the ECU may have aborted and
  begun again. An exchange the VCI ended with no response (the worker reported a receive timeout
  for the primitive) ends as `response_incomplete`, classified `undetermined`, with H2 among
  its readings unless a pending in the exchange or a matched answer excludes it; one the agent's
  own deadline ended keeps `no_response`, since the response may still be arriving (design 8.11
  `vci_did_not_end`).
  The log writes no time for the start yet: its VCI time needs per-clock epochs (ADR-353), and
  the agent's entry has no field for it. Every other item is an `unattached` event, written in
  the order read and before the exchange's entry: an item bound to an earlier primitive, a
  start-of-message indication no exchange takes as above, and a transmit indication whose header
  is not a plain 4-byte CAN identifier (extended or mixed addressing), which therefore never
  confirms a send. The confirming indication's read time is the entry's
  `times.agent.tx_confirmed_us`.
  Status and information items are not written. Each gives the `link`, the `item` (from a result item's
  RxFlag, ISO 22900-2:2022 D.2.2: `transmit_indication`, `loopback`, `start_of_message`,
  `break` or `received`; or `error_event`), the `primitive` (`none` or `earlier`), a result's
  `identifier` when its header carries one, the raw
  `vci_us` with the `clock` the worker gives the item (`device` or `worker`, ADR-353), or none
  when the worker gives none, since the agent does not infer it from the worker (design
  principle 6), the agent's `epoch` and `read_us`,
  and for a result its `rx_flag` in hex and its `message`, redacted as a request for a transmit
  indication or a loopback and as a response otherwise; for an error event its name and its
  extra error information as the worker gave it (ADR-351).
  Each ECU identity value the runner reads from a source the program declares (the hardware
  part number and software version a write job records at its plan's entry, and the VIN,
  hardware identity and software version a restart checks) is an `identity` event as it is
  read. It has the `kind`, an `outcome` (`value`, `negative_response` with its `nrc`, or
  `unreadable`), and the value as `text` when it is printable ASCII, else as `hex`. A read
  whose request failed writes none; its exchange entry says why. A `diag.log` message is a
  `procedure_log` event with its `level` and `message`; the operational log (`tracing`) records
  only that the procedure logged one, with its level.
- **Clean close and reopen.** A log was closed cleanly when it holds its header and its last
  run's lines (from that run's header or `agent_clock`) number on from the largest `seq` before
  them, with nothing blank or damaged between them, to a complete `job_end`. Each run judges only
  the run before it: damage further back was reported by the run that found it. A reopen cuts a
  torn last line, goes on after the largest `seq` the file holds, so numbering never goes back,
  and starts the epoch after the largest the file holds; when the close was not clean, its
  `agent_clock` is marked incomplete. A file that holds no header (an empty one an earlier run
  created included) gets one, marked incomplete, after whatever it holds, and so does a resume that finds no log, since the job ran before. A
  file whose `seq` or epoch has no successor is not opened (`NotOpened` with `InvalidData`). A
  new or empty file's directory is synced, as the journal's is, so its name survives a power
  loss, and on Unix a new log file is its owner's alone (mode 0600).
- **Writes.** One write per line from the job's thread, synced after the opening lines and
  after `job_end`. A failed write or sync stops the log for the rest of the run, which writes no
  `job_end`, so the next reopen sees an unclean close.
- **Status.** Every entry point returns a `JobRun`: the job's `result`, its `guards` and
  `comm_log`, a `CommLogStatus`. It is `None` without a log (none configured, a job refused
  before its thread starts, or one that ends before the log opens: a cancel, a journal another
  run holds, a resume its journal turns away), `Complete`, or `Incomplete` with the reason: `NotOpened` (a log another run holds
  is `WouldBlock`), `WriteFailed`, `UncleanReopen` or `CloseFailed`. The result is the same
  whatever the status.
- **Redaction.** Message bytes reach the log only through `comm_log::redaction::redact`. The
  service and sub-function stay in clear, and:
  - SecurityAccess (ISO 14229-1:2026 9.4): a key is its length, a seed its length and whether it
    is all zero; a request or response outside the seed and key levels (the supplier-specific
    range and the reserved values, which define no pairing) is its length in both directions;
  - Authentication (9.6): the data is its length, and a response keeps its return value;
  - TransferData (14.4): the data of a download's requests and an upload's responses is its
    length and SHA-256, the block counter in clear.

- **Exchange classification.** `comm_log::exchange` builds an exchange entry from what the
  agent saw of it (`Evidence`) and the link's history (`LinkState`: answers, matched answers,
  owed requests, the last link error, an accepted ECUReset or session change), and classifies it
  by design 8.11's table, first matching row. It decides only the rows the agent's evidence
  reaches today: a request is `sent` `confirmed` by its transmit indication (above) or by an
  answer no owed request could own, and a confirmed send with no response is `vehicle` from an
  ECU that has answered requests matched this way with no link error, `undetermined` otherwise
  (on a link whose VCI absorbs pendings, the possible-pending row below comes first); without a transmit time the timing is `not_judged`, so on a link
  whose VCI absorbs pendings every response is `undetermined` (a pending may have come first);
  an answer an owed request could own leaves the exchange that received it `undetermined`, and
  both stay owed (the earlier exchange is reclassified only by a settling entry, which comes
  later). An accepted session change or ECUReset, of any type (ADR-346 item 6), ends the matched
  answers and the owed requests, and an unusable answer to either ends only the matched
  answers. A link error ends the matched answers too, but a final response that came after
  every link error of its exchange, in the order the host read the link's items, counts as an answer
  since the error (VCI times are not compared); an error after it, or from another link of the
  channel, ends it. A reopened link, a worker
  restart or a VCI reconnect (`LinkState::reopened`) ends the matched answers and keeps the owed
  requests. The other rows stand as comments where the table puts them, and evidence only they
  could classify falls to the last row, `undetermined`, at the place in the table where that row
  stands.
- **Exchange entries.** The worker host writes one entry for each request a VM step, the runner
  or the restart sends through it (`send_recv`, the generic send-receive) and for each it refuses
  before sending (`not_sent`: the request policy, an undeclared service, TransferData outside
  `FlashTransfer`, a block with no transfer, one longer than the ECU's block length, or one
  refused for its RequestDownload's response, which `caused_by` names). A request the worker
  refused to start and named a reason for (ADR-350 item 2: invalid parameters, a full queue, a
  request the protocol cannot carry) is `not_sent` too, refused by the worker for its own
  reasons (`system_own_reasons`, `system`), with the reason's name in `detail`'s
  `worker_detail`; a failed start without a reason stays `unknown`
  (`primitive_failed_to_start`), since it does not show the request was not sent. Each has its
  `origin`, the link's identifiers as its `target`, the agent's times, and in `detail` the host
  error's variant name, the error event's name and the gRPC status code, never their text. The
  named error event's extra error information (ISO 22900-2:2022 D.4.2) goes in too, as the
  worker gave it (ADR-351): its code in `extra_error_info` and its name in
  `extra_error_info_name` (the D-PDU worker's from the library's MDF, the J2534 worker's own).
  The host does not interpret either, so neither changes the outcome or the classification. A
  lost VCI the worker reports before a request starts carries neither. A step's
  request has `origin` `procedure` and the step's `at` (pc, step count, the source position the
  program's source map gives, and the sections in force); the runner tells the host where each
  step begins and that it ended (`ProcedurePosition`). The runner's and the restart's own reads
  have no `at`, and the origin the runner names around them (`ProcedurePosition::own_reads`):
  `identification` for the identity read at a plan's entry and every identity read of a restart
  (its gates included) or a checkpoint resume, `precondition` for every precondition read (the
  gates', and the checks before the replay and before the erase), `teardown` for the ECUReset,
  `restart_check` for the default-session confirmation and the restart's other checks, and
  `guard` for the software version read. A supply voltage read is an IOCTL, not an exchange.
  The log keeps
  one `LinkState` per target for the run, so it outlives the link within the run; a later run of
  the job starts it over, since the log is not read back. A link error an entry carries is handed
  to every other target's state too. Writing an entry never reaches the job, a panic included.

## Restart classification

`agent::restart::classify` reads a job's journal with the program and decides, without
contacting anything, how the job goes on (ADR-253). The interruption point is the latest, by
step count, of the last completed step, the two transfer markers and the last request intent.

| Journal | Decision |
|---|---|
| none (`NotFound`), program with no plan, no `RecoveryRequired` section and no diagnostic primitive that is not safe to repeat (`restart::missing_journal_is_plain_start`, ADR-335 item 1) | plain start, with no journal |
| none (`NotFound`), any other program (ADR-293, ADR-335 item 1) | `OnSiteInterventionRequired` (`MissingJournal`; the journal is created before anything is sent, so a missing one cannot be told from one lost after a request that changed the ECU) |
| unreadable (corrupt, unknown format version, another job, I/O) | `OnSiteInterventionRequired` |
| a procedure-failure record (ADR-280), checked before every row below | `Failed`, with the record's position, code and detail |
| an identity-abort record (ADR-286), checked right after the procedure-failure record | `Aborted`, with the `IdentityKind`, whatever the transfer state |
| interruption point at or past a plan's recovery-required point and before its end, or inside a `RecoveryRequired` section the journal can place it in (inside a plan, on the step into a plan's entry, or at a completed plan's end); a point no later than the last post-transfer step of a completed transfer counts as at that plan's end; also, whatever the point, an intent of a `RecoveryRequired` section outside the plans with no transfer-start marker after it (ADR-291) | `OnSiteInterventionRequired` |
| no transfer-start marker | the plain procedure's classification (below): a plain start, a checkpoint-resume decision or `OnSiteInterventionRequired` |
| transfer-start marker | `Restart`, for the plan of the marker's stage, with the VM state at its entry; `OnSiteInterventionRequired` if that plan never allows a restart |

The runner then compares the journal's artifact hash with the resume's before it acts on a plain
start, a checkpoint-resume decision or a `Restart` (ADR-344, "Restart entry" below); an
`OnSiteInterventionRequired` decision stands as it is.

A restart's entry state is the newest state the journal holds if it stands at the plan's entry (a
run start included, so a plain start never inherits an earlier run's state). It is the state before
the newest only when the newest stands at the plan's post-transfer end (the state of ADR-271 item
2), is the current attempt's end-state record (the transfer's last post-transfer step is the step
that carries it) and the interruption point is that same step (no later step or request marker;
the completion and a resume record may follow). So after a journal cut after the end state, or a
crash after the completion, the entry state stays available whenever classification allows a
restart and the state check calls for a redo; when it finds the intended version with the steps
complete it decides the read-back verification instead (ADR-271), which resumes from the end state
(ADR-274). Any other newest
state that does not stand at the entry gives `OnSiteInterventionRequired` (`MissingEntryState`);
for adjacent plans, a crash after the first plan's completion with a pre-erase primitive of the
second journaled is refused (ADR-253). The journal keeps those two states only, and
`restart::plan_end_state` gives the end state for the continuation of ADR-271 item 3 only while it
is the newest (an entry state or run start recorded after it makes it stale). Only a journal that
holds no state (written before the run-start record) falls back to the program's initial state,
for an entry at instruction 0.
The run start names no request, so it does not move the interruption point; the step count
counts it. It must decode, pass `Vm::check_state` and stand at the
plan's entry; otherwise, or for a stage the program does not declare, the decision is
`OnSiteInterventionRequired`. Its step count continues after the journal's last record
(`restart::next_steps`), so the restart's records come after the old ones.

### A plain procedure

A journal with no transfer-start marker is a plain procedure's (ADR-335 items 3 to 7), after the
checks above (a failure, an abort, a stop and a `RecoveryRequired` section) have not decided it. A
started transfer keeps the flash path. `restart::classify_plain_procedure` decides in this order:

| Journal | Decision |
|---|---|
| no `Request` was ever recorded, whatever the journal's states | plain start on the same journal, counted (below) |
| the last `Request` is not older than the state of the latest checkpoint (the newest record that carries a VM state), or there is no such state | `OnSiteInterventionRequired(UnconfirmedRequest { at })`: the unsafe primitive may have reached the ECU. It holds at every later resume until a person acts |
| the latest checkpoint's state does not decode, or fails `Vm::check_state` with its step count set to `restart::next_steps` | `OnSiteInterventionRequired(InvalidEntryState)`, before the conditions below read the state |
| otherwise, from the latest checkpoint's state, one of the conditions below fails | `OnSiteInterventionRequired(NotResumable { cause })` |
| otherwise | `ResumeFromCheckpoint`, with the checkpoint's step and its state, whose step count is `restart::next_steps` and which passed `Vm::check_state` |

The comparison of the request is with the state's own step count: a checkpoint or a step state is
taken after the completed step, so its count is one past its record's, and a run start's is the
step about to run. A cancel while a request's marker was committed leaves a marker with no later
state, which reads as unconfirmed too. The conditions of the checkpoint resume are checked in this
order, and the first that fails is the cause:

1. the checkpoint's pc is an interruptible point (`restart::interruptible_point`, the rule the
   runner marks cancel points with, ADR-330): not inside a section whose `interruptible` is not
   `Yes`, and not a `Ret`. Otherwise the cause is `UninterruptibleSection { pc }`, also for a
   checkpoint standing on the return after a `No` section's last primitive (ADR-335 item 3). A
   plain procedure declares no re-entry point, so such a checkpoint never resumes (ADR-335
   item 5);
2. nothing reachable from the checkpoint is a hazard. Reachability (`Program::reachable_from`) is
   the forward closure over fallthrough, jumps, calls and returns, with the state's suspended
   callers as the returns' targets; it is conservative (both ways of a conditional jump). The
   lowest reachable pc that is one of these is the cause: an instruction in a plan's range
   (`PlanAhead { flash_session }`), a diagnostic primitive in a `RecoveryRequired` section
   (`RecoveryRequiredAhead { section, pc }`), or a diagnostic primitive that is not safe to repeat
   (`UnsafePrimitiveAhead { pc }`; one outside every section is not safe, and a loop's back edge or
   a call can make an earlier one reachable);
3. the job names a well-formed target VIN and the program declares a VIN source, otherwise
   `NoIdentity`.

A plain start on an existing journal counts as a resume of the plain procedure: the runner commits
the existing resume record under the reserved stage `diag_ir::PLAIN_PROCEDURE_STAGE` before anything
is sent, after a cancel check, and a stage already resumed `JobLimits::max_procedure_resumes` times
(agent policy, default 3, not in the IR) ends the job in
`OnSiteInterventionRequired(ProcedureResumeLimitReached { resumes, max })` with nothing sent. A
plain start of a missing journal runs without one and is not counted.

A `ResumeFromCheckpoint` decision runs in this order, each failure ending the job as stated:

1. The limit and the count, as for a plain start (`ProcedureResumeLimitReached` with nothing
   sent, then a cancel check, then the resume record), before anything is sent. A checkpoint
   resume counts under the same stage and limit (ADR-335 item 6).
2. The identity (`restart::check_procedure_identity`, ADR-261, ADR-263): the ECU's VIN is read
   through the program's VIN source, the only request sent before the run, with a cancel check
   before and after. A well-formed VIN equal to the job's takes the per-vehicle lock; another
   well-formed VIN is `JobError::IdentityMismatch`, journaled first and terminal (ADR-286); a VIN
   that cannot be established (no answer, a negative response, a malformed value, a worker
   failure) is `OnSiteInterventionRequired(ProcedureIdentityNotEstablished)`. The count stays, and
   the job can be resumed again within the limit (ADR-290).
3. The run: a run start with the restored state is committed first (ADR-274), then the VM goes on
   from that state with a step budget of its own (ADR-287). The run is journaled as a first run
   is (requests, checkpoints, a declared failure, a stop), and the job ends as that run would. The
   checkpoint's unsafe requests are not sent again; the continuation holds only primitives that
   are safe to repeat and runs in whatever session the ECU is in (ADR-335 item 8). Before any of
   this, as for every restart, a journal another run holds ends the job (`InUse`), a recorded
   failure, identity abort or stop ends it in its own error, and a target VIN or intended
   software version that differs from the journal's is `OnSiteInterventionRequired`
   (`TargetVinDiffers`, `IntendedVersionDiffers`), all with nothing sent; the artifact's hash is compared there too (below).

## Restart entry

`agent::resume_program_journaled` takes the same arguments as `run_program_journaled`,
guards included, for a job that ran before (ADR-255), and returns the guards with its result.
It opens the job's journal (`Journal::open`, with its writer lock) before it opens the worker
link; a journal another run holds, or one that records a declared failure, an identity abort or a
run's stop, ends the job there.
Once the link is open and the policy allows the program, and before anything is sent to the ECU,
it classifies the journal.

| Decision | What the runner does |
|---|---|
| plain start | on an existing journal, checks the plain stage's resume count against `JobLimits::max_procedure_resumes` (`ProcedureResumeLimitReached`), takes a cancel check, commits the resume record, then runs the program from its start on the same journal; its step count starts at `restart::next_steps`, after the journal's last record, and the identity is read again. A program whose journal is missing (a plain start only for a program that sends nothing it could not repeat, ADR-335 item 1) runs without one |
| `Failed` | ends in `JobError::ProcedureFailed` with the record's position, code and detail, read under the journal's lock before the worker link is opened; nothing is sent to the ECU and no resume is counted (ADR-280) |
| `Aborted` | ends in `JobError::IdentityMismatch` with the journaled `IdentityKind`, read under the journal's lock before the worker link is opened and before the target VIN and intended version are compared; nothing is sent to the ECU and no resume is counted (ADR-286) |
| `Stopped` | ends in the error the stopped run returned, `JobError::StepLimit` with the recorded step count or `JobError::Vm` with the recorded position and error, read under the journal's lock before the worker link is opened; nothing is sent to the ECU and no resume is counted, so a plain start does not send the pre-erase requests again and a restart does not redo the transfer (ADR-288) |
| `OnSiteInterventionRequired` | ends in `JobError::OnSiteInterventionRequired` with the classification's reason; nothing is sent |
| `ResumeFromCheckpoint` | counts the resume, reads and checks the VIN (taking the vehicle lock), commits a run start with the restored state and goes on from the checkpoint, ending as a first run would; the steps are those of "A plain procedure" above |
| `Restart` | runs `restart::check_before_ecu`, then the gates of `restart::check_gates` (which promote the guards to the per-vehicle lock once the VIN matches, ADR-263), then the teardown of `restart::teardown` (ADR-264), then the default-session confirmation of `restart::confirm_default_session` (ADR-265). An ECU that cannot be confirmed ends the job in `JobError::OnSiteInterventionRequired(DefaultSessionNotConfirmed)`. Then `restart::check_identity` (step 3a) reads the VIN and the hardware identity again: a different one ends the job in `IdentityMismatch` (journaled first, ADR-286), one that cannot be established in `OnSiteInterventionRequired(IdentityNotEstablished)`, and a matching VIN promotes the guards again (the lock's fallback when step 2 could not read it). Then `restart::check_state` (step 3b-2) reads the software version and decides between redoing the transfer and the read-back verification (`restart::check_state` below): a version nobody wrote ends the job in `OnSiteInterventionRequired(UnexpectedSoftwareVersion)`, one that cannot be established in `OnSiteInterventionRequired(SoftwareVersionNotEstablished)`. When the state check decides to redo the transfer, `restart::check_reentry` (step 4a) checks every declared precondition again through its default-session source: one that does not hold ends the job in `OnSiteInterventionRequired(PreconditionNotMet)`. When step 4a passes, the program's steps from the plan's entry boundary up to, not including, its erase run again from `RestartPoint::entry_state` (step 4b-2, ADR-273; `replay to the erase` below), and then `restart::check_before_erase` (step 4b-3) checks every declared precondition once more through its programming-session source: one that does not hold ends the job in `OnSiteInterventionRequired(PreconditionNotMetBeforeErase)` with no erase sent. After that check the same VM goes on from the erase as a first run does (step 4c, ADR-276; `redo from the erase` below), and the job ends as that first run would. When the state check decides the read-back verification, the program goes on after the plan instead (`continuation after the read-back verification` below), and the job ends as a first run would; the gates' reads, at most one ECUReset, the F186 reads, step 3a's two reads, step 3b-2's version reads, step 4a's precondition reads, the replayed steps, step 4b-3's precondition reads and then the redone transfer and the rest of the program, or after a verification the program's steps after the plan, reach the ECU |

The on-site endings that the `Restart` row reaches after `check_before_ecu` has committed the
resume count end that run, not the job for good: they are not journaled (ADR-290), and a later
resume of the job tries again, counting another
resume and running the gates again, until the count reaches the plan's `max_resumes`, which ends
the next resume in `ResumeLimitReached` with nothing sent.

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
`JobError::Journal(JournalError::InUse)` before the classification, with nothing sent; the
journal's lock is taken before the worker link is opened, so this holds also when the worker or
the VCI cannot be reached. The start
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

The flash artifact is compared the same way (ADR-344): `Journal::create` takes the hash of the
job's artifact and writes `Record::ArtifactSha256` in the creation prefix after the VIN and the
intended version, once. A resume compares `RecoveryFacts::artifact_sha256` with the hash of the
artifact it is given, right after the intended version and the classification, with nothing sent
and no resume counted. It applies only where the classification would go on (a plain start, a
checkpoint resume or a restart): an on-site decision of the classification stands, so another
artifact never turns it into a refusal. Two hashes, or a hash on one side only, differ; two absent
hashes (a job without an artifact) do not. A journal written before the record exists holds none. A
difference ends in `JobError::ArtifactDiffersFromJournal` while the journal has not reached a flash
plan (`restart::reached_flash_plan`: no transfer-start marker, and the last step and the newest
request intent outside every plan's range), and in `OnSiteInterventionRequired(ArtifactDiffers)`
once it has. An older agent reads the appended record as a corrupt journal (`UnreadableJournal`).
The job check runs first on a resume as on a first run, so a program that reads blocks and is given
no artifact, or one too short for a segment, ends in `NoArtifact` or `SegmentOutsideArtifact` with
the journal untouched, and a resume given the job's own artifact can still go on.

`check_gates` is ADR-229 item 2 step 2 before the teardown (ADR-261). It reads only, and stops
at the first gate that does not hold:

1. The VIN, through the program's VIN source, against the VIN the job names (the journal's
   target VIN). A well-formed VIN (17 characters, each a digit or an upper-case letter other
   than I, O and Q) other than the job's ends the job in `JobError::IdentityMismatch`, journaled
   first (ADR-286). No
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

1. The journal's exclusions. Post-transfer steps journaled complete for the interrupted pass
   (the completed path, `restart::completed_pass_interrupted`, the rule the state check applies,
   ADR-269 item 2) give `Teardown::CompletedPath`: no reset and no wait, since the
   default-session confirmation of step 2b-2 comes first (ADR-265). A RequestTransferExit intent
   without the completion gives `Teardown::Passive(TransferExitJournaled)`. A completion left
   by an earlier pass is neither: when the program stepped back into the plan, or a redone
   transfer's replay (ADR-273) journaled a step before a crash, that pass's RequestTransferExit
   was answered and its post-transfer steps finished, so the gate decides. A replay that
   crashed with only its run start committed changes no fact, so the earlier pass still counts
   as the interrupted one there; the confirmation and its passive retry cover an ECU the replay
   left in its programming session.
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
  `JobError::IdentityMismatch { identity: Vin }`, journaled first (ADR-286). Anything else (no target VIN or no declared
  source, no answer, a negative response, a value that is not a well-formed VIN, a worker
  failure) ends it in `OnSiteInterventionRequired(IdentityNotEstablished { flash_session,
  identity: Vin, teardown })`, where `teardown` is step 2b-1's outcome, so the technician
  knows whether an ECUReset was sent;
- the hardware identity, only after the VIN matched: the raw field bytes must equal the
  journal's. Different bytes end the job in `IdentityMismatch { identity: HardwarePartNumber }`
  (journaled first, ADR-286),
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
the erase, so no transfer-start marker is committed. The replay first commits a run start carrying
the entry state (ADR-274), so a crash during it restarts from the same entry state even when the
journal's newest state was a completed pass's end state.
It shares the step loop with a run from instruction 0 (`run_vm`) and ends as a first run does on a
failed step, a wait nobody answers, the step limit or a cancel.

`restart::check_before_erase` is step 4b-3 (ADR-229 item 2 step 4, ADR-245 item 6), run when the
replay stopped at the erase. It checks every declared precondition (supply voltage, external
supply, ignition, engine, vehicle speed; the VIN and the hardware identity are not preconditions
and are not read) in the order of step 4a and stops at the first that does not hold, reading each
through its `programming_session` source only. That source must be usable in the programming
session (ADR-229 item 2 step 4), which the plan's replayed steps normally enter. The agent tracks
no session: when the steps from the entry re-enter no programming session, the ECU is still in its
default session here, and a source that answers only in the programming session fails the check,
with nothing erased.
A missing source (which `Program::validate` refuses for a restartable plan), a value outside the
declared range, a reading that is not a value, a source the table does not map and a worker
failure all mean not met. Each read is made once; a cancel stops it at the start and around each
read. A failure ends the job in `OnSiteInterventionRequired(PreconditionNotMetBeforeErase {
flash_session, precondition, teardown })`, a reason apart from step 4a's `PreconditionNotMet`: it
tells the technician that the plan's steps from its entry were replayed (which normally puts the
ECU in its programming session) and that no erase was sent. `teardown` is the step 2b-1 outcome, from before
the replay.

### Redo from the erase

When step 4b-3 passes, the replay's VM stands at the erase, with nothing at it journaled or sent,
and the same VM goes on from there as a first run does (step 4c, ADR-276), with the replay's
journaling:

- its arrival at the erase commits a new transfer-start marker before the erase is sent. In the
  journal that starts a new attempt: the earlier attempt's RequestTransferExit marker and
  post-transfer progress go with it, so an interruption of the redo takes the ordinary restart
  order (for example, an ECUReset is no longer ruled out by the earlier exit marker);
- the erase, the RequestDownload, the blocks, RequestTransferExit, the post-transfer steps and the
  rest of the program then run and are journaled as on a first run, and the job ends as that run
  would;
- the agent tracks no session and does not judge the erase's answer, on a redo or on a first
  run (ADR-276 item 3). A plan whose steps from its entry to its erase enter no programming
  session sends the erase in the default session the restart confirmed, and the job goes on as
  the procedure handles the answer. No block is sent without a positive RequestDownload: an ECU
  that refuses the download without a successful erase, as `sim-ecu` does, ends a procedure that
  ignores a refused erase at the first block with nothing written, while one that accepts the
  download is written, as on a first run. A lost answer to the erase ends the job at the erase.
  Either way the transfer-start marker is
  already committed, so a further restart takes the restart order for that new attempt, within
  the resume limit, which the new marker does not reset;
- the step limit applies to each run (ADR-287): the restart's step count continues after the
  journal's last record for the journal's ordering, but the limit counts only the restart's own
  steps, the replay and the redo together, so a redo has the budget a first run has. A restart
  the limit stops journals the stop and is not resumed again (ADR-288).

The restart order up to the erase is `runner::restart_order`; `resume_on` continues from where it
stopped. In test builds `OnSiteReason::StoppedBeforeErase` reports a restart that the tests
stopped before the erase, with the teardown's outcome, the confirmation and the state check; a
job never ends in it.

The restart order is tested end to end against `sim-ecu` in `j2534-0404-service`
(`tests/agent_restart_end_to_end.rs`): the agent runs as a child process that is killed after a
block of the transfer was journaled, and `resume_program_journaled` resumes the job from the same
journal directory, both within the ECU's session timer and after it ran out. A third test kills
only the worker at the same point: the run fails on the lost worker and marks its guards
unconfirmed (ADR-258); once the worker is reaped the mark is cleared with `JobGuards::worker_gone`,
and the next run resumes on those guards with a new worker and takes the same restart order. Two
more interrupt the run at the same point without killing anything: the ECU loses power
(`sim_ecu::Fault::PowerLoss`) or the VCI is unplugged (`disconnect_vci`, ADR-238). The next
block's transfer gets no answer, or finds the link offline because the worker met the lost device,
and the run ends there, keeping its guards and its journal; after the disconnect the guards' link is
unconfirmed as after a worker crash. The next run, once power is back or on a new worker that finds
the VCI plugged in again, takes the same restart order and completes. When the VCI is still
unplugged at the next run, its worker cannot open the device (`ERR_DEVICE_NOT_CONNECTED`), so the
run ends in `JobError::Link` before its link opens: nothing is sent, no resume is counted, the
journal is unchanged and the guards are given back still held; the run after it, with the VCI
back, completes the job. Three more tests kill the
agent after RequestTransferExit was sent: before its response was recorded, after it but before the
post-transfer step ran, and after the post-transfer step was sent but before its response was
recorded. The restart's teardown is passive (no ECUReset), and it redoes
the transfer, so RequestTransferExit and the post-transfer steps run again in order. Four more
cover the teardown's other paths, each completing the job: an ECUReset the ECU refuses or whose
response is lost (passive teardown, `ResetRefused` and `ResetOutcomeUnknown`), a VIN the gate
cannot read (passive teardown, `Gate`), and an ECU that acknowledges the reset and then stays
silent past the confirmation window (confirmed after the passive retry). Three more kill the agent
after the post-transfer steps were journaled complete, so the teardown takes the completed path (no
reset, no wait), and make the confirmation's first F186 read fail by each of its triggers: the
programming session still active, a refused read, an unanswered one. Each is confirmed after the
passive retry and completes the job. These restarts redo the transfer: the test job declares no
software version to write, so the state check cannot tell the download took. One more runs a job
that declares the version `sim-ecu` installs at the plan's own closing ECUReset and kills the agent
after the plan: the restart finds that version with the post-transfer steps journaled complete,
which is the read-back verification, sends no erase, transfer or reset, and goes on after the plan
from the VM state journaled at its end. The
tests check the restart's requests in order against `sim-vci`'s request trace (`NGR_SIM_VCI_TRACE`)
and that the restart power-cycled the ECU exactly once (not at all when it sends no ECUReset). The
kill-and-restart harness is `tests/common/restart.rs`.

### Continuation after the read-back verification

When `restart::check_state` decides `ReadBackVerification`, the match is the verification
(ADR-271 item 1) and the program goes on after the plan (`runner::continue_after_plan`, ADR-271
items 3 to 5): no erase and no ECUReset are sent for the verified plan. Before anything else is
sent:

- `restart::unsafe_continuation` looks at two kinds of diagnostic primitive; sections may
  overlap, and every section that covers a primitive counts:
  - one outside all of the program's plan ranges (`entry_pc` up to, not including,
    `post_transfer_end_pc`), statically over the whole program. The journal records no step
    there, so it may have reached the ECU before the interruption. It must lie in a section,
    and every section over it must be `Safe` and none marked `RecoveryRequired`, since the
    journal cannot rule out that the interruption was there;
  - one in the window of the plan that begins at the verified plan's end: from its entry up to
    its erase or its recovery-required point, whichever comes first. A crash with its request in
    flight, before that plan's first journaled step, leaves the same journal as a crash at the
    verified plan's end, so the continuation sends it again; from the recovery-required point
    on, the first request gets an intent before it is sent. For a plan that allows a restart that is the
    replay of ADR-273 item 5, and only an `Unsafe` section refuses (`Program::validate` already
    excludes one). For a plan that does not, the window is checked like the primitives outside
    the plans. A plan reached after that one is not looked at: its predecessor's erase journals
    a transfer first, so an interruption there restarts that plan.

  The first primitive that fails ends the job in
  `OnSiteInterventionRequired(UnsafeContinuation { flash_session, pc, teardown, confirmed })`.
  A cancel that arrived before these checks ends the job in `Cancelled` instead.
- `restart::plan_end_state` gives the VM state at the plan's end. None, a stale one (an entry
  state or a run start recorded after it) or one that fails `Vm::check_state` ends the job in
  `OnSiteInterventionRequired(MissingPlanEndState { flash_session, teardown, confirmed })`. This
  covers a redo whose replay committed only its run start before a crash, after an earlier pass
  had completed: the facts still report the completed pass, but the job is not resumed from a
  state older than the journal's newest.

Both reasons tell the technician that the image is verified. Otherwise the VM resumes from the end
state, with its step count after the journal's last record, and runs to the program's end with a
first run's journaling: a later plan journals as on a first run, and a step that fails, a cancel or
the step limit (counted from the continuation's first step, ADR-287) end the job as on a first run. The continuation commits no run start (ADR-274 item
4), since a run start that is not at the entry would leave a later redo without its entry state.

## Job guards

The `guards` module holds a job's exclusive locks on the device (design 8.8, 8.8.1; ADR-256,
ADR-257, ADR-262): the per-VCI lock, the device's single reprogramming slot and the per-vehicle
lock. Each is an OS lock (`File::try_lock`) on a file in the device's lock directory: `vci-{hex
name}.lock`, `reprogramming.lock` and `vehicle-{k:03x}.lock`. The OS releases a lock when its
process dies, so a crashed run never blocks the next one, and the files are never deleted.

- `JobGuards::take(GuardSetup { dir, vci }, poll, cancelled)` takes the per-VCI lock, then the
  slot, for a job that writes (`Job::writes`). It waits while another job holds either,
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
the guards by value and returns them with the run's result (`JobRun`), so two runs can never use one set
at once. A program that writes and that this build allows (a debug build, on the simulator),
run on guards without the slot, ends in `JobError::NoReprogrammingSlot` before anything opens;
a release build refuses every writing program first (`JobError::Refused`). A new run after an agent crash or a loss of the
device's power takes them with `JobGuards::take` before it calls the runner, so a duplicate
resume of the same job waits there without opening a link. A dropped future releases them only
once the job thread has ended. A cancel (a dropped future) is taken only at an interruptible point
(design 5.6, 8.10.1, ADR-330): one that comes inside an uninterruptible or a recovery-required
section is held, the section runs to its end, and the job stops before the first instruction
outside every such section that is not a `Ret`, so a procedure that ends first finishes. The
start of a run from the program's first instruction (a new run, or a plain start on an existing
journal) is an interruptible point even when `main` is itself such a section, so a cancel that
came in before the first step ends the job as cancelled with nothing sent; a checkpoint resume and
a restart's replay keep the rule above (ADR-337). The
job thread, with its guards and link, may therefore run on for as long as the section takes,
waits included, and the worker must not be handed to another job meanwhile. A job that survives a worker crash, a VCI disconnect or a loss
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
  big-endian integer, or printable ASCII). A job with a document gets its table from the
  document (`ServiceSources::from_document`, ADR-345): one entry per distinct
  `EcuService` source the program names (identity and precondition sources), each read from the
  declared service, with the request encoded as the runtime encodes it. The table reads the
  field's raw bytes: a conversion the field declares is not applied, since a precondition range
  and an identity check compare the value as read. `check_job` builds it, so
  a source the document does not declare, or declares as anything but a single-identifier
  ReadDataByIdentifier whose positive response echoes the identifier and ends in one
  fixed-length, byte-aligned ASCII or big-endian unsigned field, ends the job in
  `JobError::Sources` before anything is sent (terminated and variable-length fields are not
  read). So does a field whose kind does not fit the role that reads it: the VIN must be 17
  bytes of ASCII, a precondition source an unsigned integer, and the hardware part number and
  software version either; a source shared by two roles must fit both. The runner then uses that
  table, on a first run and a resume alike, in place of
  `JournalSetup::sources`; a job without a document keeps the caller's table (hand-built
  programs and tests). A table accepts only single-identifier ReadDataByIdentifier entries
  (request `[0x22, hi, lo]`, offset 2): other services do not echo
  their request in a form the table can check, and a request for several identifiers lets the
  ECU leave some out. Building a table also refuses a duplicate id pair. The response must be
  exactly the positive SID, the echoed identifier and the field. A source missing from the
  table, a negative, short or long response, a wrong echo, or a field that does not fit its
  encoding reads as "cannot be decoded". ASCII text keeps surrounding spaces as they are; a
  field of only spaces, or with any byte outside 0x20 to 0x7E, cannot be decoded.

A program's `ReadInput` (what `diag.precondition` lowers to for a runtime-input source,
ADR-338) reaches the same inputs through `DiagHost::runtime_input`, which `WorkerHost`
implements with `RuntimeInputs::read`. Only an integer reading is a value. `CannotBeEstablished`,
`CannotBeDecoded` and a text reading are `HostError::InputNotEstablished(input)`, and so a host
error of the job: the runner records no stop for it (it is not a `VmError`, ADR-288), the VM state
stays at the instruction, and the job can be resumed on a VCI that can read the input. A worker failure while reading
(a D-PDU worker's internal error for an unknown name among them) is the worker's own `HostError`,
a host error of the job in the same way. The
restart's own checks read the same sources and still treat any non-integer reading as not met.

## The worker client

The runner, the host and the link set-up call the worker through the trait `agent::client::WorkerApi`
(ADR-334): one method per RPC they use, with the generated gRPC client's signatures, and
`held_module()`. `ngr-agent` uses `worker_host::client::WorkerClient`, which implements the trait by
delegating, so a production call is unchanged. A client that holds the worker's module itself
(`held_module()` is `Some`) is used by the hardware harness's guarded client
(`ngr_hw_checks::guard::GuardedClient`, design 13.5): the link set-up then neither lists, connects
nor disconnects the module, a failed open never disconnects it, and its owner closes it. The default
(`None`) is the production behaviour: the job opens and closes the worker's only module.
