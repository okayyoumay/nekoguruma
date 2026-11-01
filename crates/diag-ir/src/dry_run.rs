//! Dry run of a procedure with vehicle access mocked (design 12.1, ADR-328).
//!
//! [`dry_run`] steps a [`Program`] with the declaration part of its variant against a host that
//! sends nothing. Each request is resolved to the declared service it belongs to, answered from
//! a [`Script`] or, without a scripted answer, with a positive response built from the
//! service's declaration, and decoded through that declaration. The [`DryRunReport`] lists the
//! requests with their answers and decoded values, the other primitives the procedure reached,
//! and how the run ended.
//!
//! A variant that declares no SecurityAccess service still lets `diag.security_access` run: the
//! requests the frontend lowers it to are the UDS profile's, not a declared service's (ADR-390).
//! They are found by their place around an `Op::SecurityAccess`: the nearest `ServiceRequest`
//! before it is the seed request and the nearest one after it is the send-key request, each only
//! when no other host-facing instruction, call or return lies between. When such a request
//! belongs to no declared service, the dry run answers it in the profile's shape and reports it
//! under [`PROFILE_SEED_REQUEST`] or [`PROFILE_KEY_REQUEST`]; a declared service still answers
//! first, and any other undeclared request ends the run.

use std::collections::HashMap;

use crate::declaration::{
    DataKind, EcuVariant, Field, LengthKind, Placement, PositiveResponse, RepeatCount, Service,
    Shape, Termination,
};
use crate::decode::{DecodeError, DecodedField, Layouts, PhysicalValue, decode_positive_response};
use crate::hmi::{self, HmiForm};
use crate::{
    DiagHost, Failure, Op, Program, ProgramError, RuntimeInput, StepError, StepOutcome, Vm,
    VmError, WaitingOn, suppresses_positive_response,
};

/// The most bytes of requests, answers, forms and messages one report holds,
/// each event counted as at least [`EVENT_COST`] bytes, so a definition run at ingestion cannot
/// make the report take unbounded memory (ADR-328 item 7).
pub const MAX_REPORT_BYTES: usize = 64 << 20;

/// What one event counts toward [`MAX_REPORT_BYTES`] besides its bytes: twice its size in the
/// events list, since the list's storage can grow to twice its length.
pub const EVENT_COST: usize = 2 * size_of::<DryRunEvent>();

/// The `service` of a [`DryRunEvent::Request`] for a seed request the dry run answered as the UDS
/// profile's, with no declared service (ADR-390). The colon keeps it from being any
/// `Service.short_name`.
pub const PROFILE_SEED_REQUEST: &str = "profile:security_access_seed";

/// Like [`PROFILE_SEED_REQUEST`], for the send-key request.
pub const PROFILE_KEY_REQUEST: &str = "profile:security_access_key";

/// The digest a [`DryRunEvent::FlashTransfer`] carries: 64-bit FNV-1a over the block's bytes.
/// Not cryptographic; it tells blocks apart in a report and nothing more.
pub fn block_digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The seed a built answer to the profile's seed request carries; not zero, which on UDS means
/// the ECU is already unlocked.
const BUILT_SEED: [u8; 4] = [1, 2, 3, 4];

/// A built positive answer to a seed request (ADR-390 item 3, ADR-405): the positive SID, the
/// level, then [`BUILT_SEED`] cut or repeated to `seed_len` bytes when the declaration fixes the
/// seed's length. It is never all zero for a seed of one byte or more, so the procedure takes
/// the key path.
fn built_seed_response(positive_sid: u8, level: u8, seed_len: Option<usize>) -> Vec<u8> {
    let seed = BUILT_SEED.iter().copied().cycle();
    let mut bytes = vec![positive_sid, level];
    match seed_len {
        Some(len) => bytes.extend(seed.take(len)),
        None => bytes.extend_from_slice(&BUILT_SEED),
    }
    bytes
}

/// Every [`RuntimeInput`]; [`Script::built_defaults`] gives each a value. Its `match` fails to
/// compile when a new input is added without a value, and a test that decodes every variant tag
/// fails when the input is missing from this list.
const ALL_INPUTS: [RuntimeInput; 5] = [
    RuntimeInput::SupplyVoltageMillivolts,
    RuntimeInput::ExternalSupplyConnected,
    RuntimeInput::IgnitionOn,
    RuntimeInput::EngineRunning,
    RuntimeInput::VehicleSpeedKmh,
];

/// The answers a dry run gives. Each list is answered in order, and its last answer again once
/// the list is used up, so a procedure that polls gets a steady answer.
///
/// The profile's SecurityAccess requests (module docs) are answered from [`Script::seed`] and
/// [`Script::key`]; a scripted response for the send-key request itself is out of scope: its
/// answer is always built.
#[derive(Debug, Clone, Default)]
pub struct Script {
    responses: HashMap<String, Vec<Vec<u8>>>,
    seeds: Vec<Vec<u8>>,
    keys: Vec<Vec<u8>>,
    forms: Vec<Vec<u8>>,
    records: Vec<Vec<u8>>,
    inputs: Vec<(RuntimeInput, i64)>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// The script for a run on answers built from the declarations alone, as the conversion
    /// dry-runs a procedure (ADR-391): a key of four zero bytes, the empty positive answer to
    /// every HMI form and record template, and 0 for every [`RuntimeInput`]. Nothing is scripted
    /// for a service, so each request is answered from its declaration. Like the zero answers of
    /// ADR-328 item 3, the values are placeholders that make no claim about a vehicle.
    pub fn built_defaults() -> Self {
        let script = Self::new()
            .key([0u8; 4])
            .form(hmi::answered(""))
            .record(hmi::answered(""));
        // Exhaustive, so a new input must be given its value here.
        let inputs = |input: RuntimeInput| match input {
            RuntimeInput::SupplyVoltageMillivolts
            | RuntimeInput::ExternalSupplyConnected
            | RuntimeInput::IgnitionOn
            | RuntimeInput::EngineRunning
            | RuntimeInput::VehicleSpeedKmh => (input, 0),
        };
        Self {
            inputs: ALL_INPUTS.into_iter().map(inputs).collect(),
            ..script
        }
    }

    /// Adds a whole response, starting with its SID, to the answers to the service named
    /// `service` (`Service.short_name`). A negative response may be scripted too; it is the
    /// final answer, so a response-pending code, which the worker absorbs on a real link, is
    /// not one to script.
    pub fn response(mut self, service: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.responses
            .entry(service.to_owned())
            .or_default()
            .push(bytes.into());
        self
    }

    /// Adds a whole response, SID included, to the answers to the profile's seed request, in
    /// place of the built `positive SID, sub-function, 01 02 03 04` answer. A negative response may be
    /// scripted; the call then ends as it would on a vehicle. It applies only when no declared
    /// service answers the request: when a declared service does, it is not used, and that
    /// service's [`Script::response`] list answers both requests.
    pub fn seed(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.seeds.push(bytes.into());
        self
    }

    /// Adds a key to the answers to `SecurityAccess`, in place of the server's key (8.10).
    pub fn key(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.keys.push(bytes.into());
        self
    }

    /// Adds an operator's answer to an HMI form.
    pub fn form(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.forms.push(bytes.into());
        self
    }

    /// Adds an operator's answer to a record template (4.3.1).
    pub fn record(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.records.push(bytes.into());
        self
    }

    /// Adds the answer to an HMI form as the host builds it (ADR-340 item 3): `text` the operator
    /// entered when `ok`, a decline otherwise.
    pub fn form_answer(self, ok: bool, text: &str) -> Self {
        self.form(if ok {
            hmi::answered(text)
        } else {
            hmi::declined()
        })
    }

    /// Adds the answer to a record template as the host builds it: the server's judgment of the
    /// record (`ok`, `ng` or empty) when `ok`, a decline otherwise.
    pub fn record_answer(self, ok: bool, text: &str) -> Self {
        self.record(if ok {
            hmi::answered(text)
        } else {
            hmi::declined()
        })
    }

    /// Adds an HMI timeout as the answer to an HMI form.
    pub fn hmi_timed_out(self) -> Self {
        self.form(hmi::timed_out())
    }

    /// Sets the value `Op::ReadInput` of `input` reads (ADR-338), in the unit the precondition
    /// checks use: millivolts, 0 or 1 for a flag, km/h. A later call for the same input replaces
    /// the value. An input that is not set is one the run cannot establish
    /// (a [`DryRunError`], as on a vehicle that offers none): a dry run invents no measurement.
    pub fn input(mut self, input: RuntimeInput, value: i64) -> Self {
        self.inputs.retain(|(set, _)| *set != input);
        self.inputs.push((input, value));
        self
    }
}

/// What the procedure did, in order.
#[derive(Debug, PartialEq)]
pub enum DryRunEvent {
    /// A request sent by `ServiceRequest`, `SendReceive`, `ReadDtc` or `RoutineControl`, as the
    /// whole request starting with its SID.
    Request {
        request: Vec<u8>,
        /// `Service.short_name` of the declared service the request belongs to, or
        /// [`PROFILE_SEED_REQUEST`] / [`PROFILE_KEY_REQUEST`] for the profile's SecurityAccess
        /// requests.
        service: String,
        answer: Vec<u8>,
        /// Whether the answer was built from the declaration rather than scripted.
        built: bool,
        /// The answer decoded through the service's positive response; `None` when the service
        /// declares none, and for the profile's requests, which have no declaration.
        decoded: Option<Result<Vec<DecodedField>, DecodeError>>,
    },
    SecurityAccess {
        level: u8,
        seed: Vec<u8>,
        key: Vec<u8>,
    },
    /// A block sent with `FlashTransfer`. The report keeps its length and a digest of its
    /// bytes ([`block_digest`]), not the bytes: a dry run serves a block as zeros (ADR-336 item
    /// 15), and a flash image can be larger than the report's budget (ADR-391).
    FlashTransfer {
        block: u32,
        len: u64,
        digest: u64,
    },
    /// Passes at once: a dry run does not wait.
    Wait {
        millis: u32,
    },
    /// The form the VM sent, decoded (ADR-340 item 7), and the scripted answer as it was
    /// pushed.
    Hmi {
        form: HmiForm,
        answer: Vec<u8>,
    },
    RecordInput {
        template: HmiForm,
        answer: Vec<u8>,
    },
    MonitorCapture {
        back_millis: u32,
    },
    Log {
        level: u8,
        message: String,
    },
}

/// The most bytes of a request, or of a service name, a [`DryRunError`] keeps: errors end the
/// run outside the report budget, so long ones are cut short (ADR-328 item 7).
pub const MAX_ERROR_BYTES: usize = 64;

/// The most service names [`DryRunError::Ambiguous`] lists.
pub const MAX_ERROR_SERVICES: usize = 8;

/// `service`'s name cut to at most [`MAX_ERROR_BYTES`] bytes at a character boundary, for an
/// error.
fn error_name(service: &Service<'_>) -> String {
    let name = service.short_name().unwrap_or_default();
    let mut end = name.len().min(MAX_ERROR_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// The first [`MAX_ERROR_BYTES`] bytes of `request`, for an error.
fn error_request(request: &[u8]) -> Vec<u8> {
    request[..request.len().min(MAX_ERROR_BYTES)].to_vec()
}

/// Why the mocked vehicle access could not answer. A `request` here is the request's first
/// [`MAX_ERROR_BYTES`] bytes, and a service name its first [`MAX_ERROR_BYTES`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DryRunError {
    /// No declared service has the request's SID and a prefix the request starts with.
    #[error("the request {request:02X?} belongs to no declared service")]
    Undeclared { request: Vec<u8> },
    /// Several declared services match the request equally well; at most
    /// [`MAX_ERROR_SERVICES`] of them are named.
    #[error("the request {request:02X?} matches several declared services: {services:?}")]
    Ambiguous {
        request: Vec<u8>,
        services: Vec<String>,
    },
    /// A `SendReceive` names a service the variant does not declare with a request and a
    /// positive response, as the agent's host requires (ADR-322 item 3).
    #[error("no declared service with a request and a positive response has id {service}")]
    UnknownService { service: u32 },
    /// A `SendReceive` request is empty or does not start with its service's declared SID,
    /// which the agent's host refuses (ADR-322 item 1).
    #[error("the request {request:02X?} does not start with service {service}'s SID {sid:02X}")]
    WrongService {
        service: String,
        sid: u8,
        request: Vec<u8>,
    },
    /// A `ServiceRequest` operand wider than one byte, which is no SID.
    #[error("service {service:#X} is not a one-byte SID")]
    NotASid { service: u16 },
    /// The service has no declared positive response and no answer is scripted.
    #[error("service {service} has no positive response to build an answer from")]
    NoPositiveResponse { service: String },
    /// The script has no answer of this kind.
    #[error("the script has no {what}")]
    NotScripted { what: &'static str },
    /// The host was sent bytes that are not an encoded [`HmiForm`], which the VM never sends.
    #[error("the HMI form sent is not an encoded form")]
    BadHmiForm,
    /// The report would grow past [`MAX_REPORT_BYTES`].
    #[error("the report would hold more than {MAX_REPORT_BYTES} bytes")]
    ReportTooLarge,
}

/// How a dry run ended.
#[derive(Debug, PartialEq)]
pub enum DryRunEnd {
    Finished,
    /// The procedure ended itself as failed (`Op::Fail`, ADR-280).
    Failed(Failure),
    /// [`Program::validate`] refused the program before it ran.
    Invalid(ProgramError),
    Vm(VmError),
    Host(DryRunError),
    /// The VM waited although the host answered; a host that answers never leaves it waiting.
    Waiting(WaitingOn),
    /// The step limit was reached first.
    StepLimit,
}

#[derive(Debug, PartialEq)]
pub struct DryRunReport {
    pub events: Vec<DryRunEvent>,
    pub end: DryRunEnd,
    /// Instructions executed.
    pub steps: u64,
    /// The pc the run ended at. The VM leaves it on the instruction that failed or stopped the
    /// run, so it locates the end in the source through [`Program::source_map`]; 0 when the
    /// program was refused before it ran.
    pub pc: u32,
}

/// Runs `program` on `variant` with vehicle access mocked by `script`, for at most `max_steps`
/// instructions.
pub fn dry_run(
    program: &Program,
    variant: &EcuVariant<'_>,
    script: &Script,
    max_steps: u64,
) -> DryRunReport {
    let mut host = ScriptedHost {
        variant,
        script,
        used: HashMap::new(),
        marks: profile_requests(program),
        pc: 0,
        events: Vec::new(),
        report_bytes: 0,
        over_budget: None,
    };
    if let Err(error) = program.validate() {
        return DryRunReport {
            events: Vec::new(),
            end: DryRunEnd::Invalid(error),
            steps: 0,
            pc: 0,
        };
    }
    let mut vm = Vm::new(program);
    let end = loop {
        if vm.state.steps >= max_steps {
            break DryRunEnd::StepLimit;
        }
        host.pc = vm.state.pc;
        let outcome = vm.step_with_declarations(program, Some(variant), &mut host);
        if host.over_budget.is_some() {
            break DryRunEnd::Host(DryRunError::ReportTooLarge);
        }
        match outcome {
            Ok(StepOutcome::Continue) => {}
            Ok(StepOutcome::Finished) => break DryRunEnd::Finished,
            Ok(StepOutcome::Failed(failure)) => break DryRunEnd::Failed(failure),
            Ok(StepOutcome::Waiting(on)) => break DryRunEnd::Waiting(on),
            Err(StepError::Vm(error)) => break DryRunEnd::Vm(error),
            Err(StepError::Host(error)) => break DryRunEnd::Host(error),
        }
    };
    DryRunReport {
        events: host.events,
        end,
        steps: vm.state.steps,
        // A `log` past the budget cannot fail, so the step completes and moves the pc on; the
        // report names the instruction that went over.
        pc: host.over_budget.unwrap_or(vm.state.pc),
    }
}

/// A request of the UDS profile's SecurityAccess call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileRequest {
    Seed,
    Key,
}

/// Whether `op` ends a scan from a `SecurityAccess` for its requests: any instruction that
/// reaches the host, and a call or return, which leaves the straight-line code of one function.
fn ends_profile_scan(op: &Op) -> bool {
    matches!(
        op,
        Op::ServiceRequest { .. }
            | Op::SendReceive { .. }
            | Op::ReadDtc { .. }
            | Op::RoutineControl { .. }
            | Op::SecurityAccess { .. }
            | Op::FlashTransfer { .. }
            | Op::Wait { .. }
            | Op::WaitFromStack
            | Op::HmiRequest { .. }
            | Op::HmiRequestFromStack { .. }
            | Op::RecordInput { .. }
            | Op::MonitorCapture { .. }
            | Op::Log { .. }
            | Op::LogFromStack { .. }
            | Op::Fail { .. }
            | Op::FlashBlock { .. }
            | Op::ReadInput { .. }
            | Op::Call(_)
            | Op::Ret
    )
}

/// The seed and send-key requests of `program`'s `SecurityAccess` steps, by pc (ADR-390): for
/// each, the nearest `ServiceRequest` before it and the nearest after it, when
/// [`ends_profile_scan`] holds for no instruction between.
fn profile_requests(program: &Program) -> HashMap<u32, ProfileRequest> {
    let mut marks = HashMap::new();
    let code = &program.code;
    for (at, op) in code.iter().enumerate() {
        let Op::SecurityAccess { .. } = op else {
            continue;
        };
        let near = |mut range: Box<dyn Iterator<Item = usize>>| {
            range
                .find(|&pc| ends_profile_scan(&code[pc]))
                .filter(|&pc| matches!(code[pc], Op::ServiceRequest { .. }))
        };
        let seed = near(Box::new((0..at).rev()));
        let key = near(Box::new(at + 1..code.len()));
        for (pc, request) in [(seed, ProfileRequest::Seed), (key, ProfileRequest::Key)] {
            if let Some(pc) = pc.and_then(|pc| u32::try_from(pc).ok()) {
                marks.entry(pc).or_insert(request);
            }
        }
    }
    marks
}

/// Which list of the script an answer comes from, for the count of answers used.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Answers {
    Response(String),
    Seed,
    Key,
    Form,
    Record,
}

struct ScriptedHost<'s, 'v> {
    variant: &'s EcuVariant<'v>,
    script: &'s Script,
    used: HashMap<Answers, usize>,
    /// The profile's SecurityAccess requests by the pc of their `ServiceRequest`.
    marks: HashMap<u32, ProfileRequest>,
    /// The pc of the instruction being stepped.
    pc: u32,
    events: Vec<DryRunEvent>,
    /// What the events count toward [`MAX_REPORT_BYTES`].
    report_bytes: usize,
    /// The pc of the instruction whose event was refused for the budget, if any; the run ends
    /// after that step, and the report's pc is this one.
    over_budget: Option<u32>,
}

impl ScriptedHost<'_, '_> {
    /// Records `event`, which holds `bytes` bytes, unless the report would grow past
    /// [`MAX_REPORT_BYTES`]. Called before the event's bytes are copied.
    fn charge(&mut self, bytes: usize) -> Result<(), DryRunError> {
        self.charge_total(bytes.saturating_add(EVENT_COST))
    }

    /// Counts `cost` toward the budget, as [`Self::charge`] does but without the per-event cost.
    fn charge_total(&mut self, cost: usize) -> Result<(), DryRunError> {
        let total = self.report_bytes.saturating_add(cost);
        if total > MAX_REPORT_BYTES {
            self.over_budget.get_or_insert(self.pc);
            return Err(DryRunError::ReportTooLarge);
        }
        self.report_bytes = total;
        Ok(())
    }

    /// The next answer from `list`, or its last once it is used up.
    fn next(&mut self, which: Answers, list: &[Vec<u8>]) -> Option<Vec<u8>> {
        let last = list.len().checked_sub(1)?;
        let used = self.used.entry(which).or_default();
        let answer = list[(*used).min(last)].clone();
        *used += 1;
        Some(answer)
    }

    /// Answers `request` as `service`, records it, and returns the answer.
    /// `seed` is set for the request marked as the profile's seed request: a built answer then
    /// carries a seed that is not zero (ADR-405).
    fn answer(
        &mut self,
        request: &[u8],
        service: Service<'_>,
        seed: bool,
    ) -> Result<Vec<u8>, DryRunError> {
        let name = service.short_name().unwrap_or_default().to_owned();
        let script = self.script;
        let scripted = script.responses.get(&name).map_or(&[][..], Vec::as_slice);
        // A request whose positive response is suppressed is answered with no bytes unless the
        // script gives an answer, as the host returns the ECU's silence (ADR-407).
        let silent = suppresses_positive_response(request);
        let (answer, built) = match self.next(Answers::Response(name.clone()), scripted) {
            Some(answer) => (answer, false),
            None if silent => (Vec::new(), true),
            None => {
                let positive =
                    service
                        .positive()
                        .ok_or_else(|| DryRunError::NoPositiveResponse {
                            service: error_name(&service),
                        })?;
                let built = built_response(&positive, self.variant.layouts());
                let answer = match (seed, request.get(1)) {
                    // The declaration's own fields after the SID and the level are the seed.
                    (true, Some(&level)) => {
                        let fixed = built.len().checked_sub(2).filter(|&len| len > 0);
                        built_seed_response(positive.sid(), level, fixed)
                    }
                    _ => built,
                };
                (answer, true)
            }
        };
        let decoded = service
            .positive()
            .filter(|_| !(silent && answer.is_empty()))
            .map(|positive| decode_positive_response(&positive, self.variant.layouts(), &answer));
        let decoded_bytes = match &decoded {
            // Twice the count: the decoder's vectors and strings grow as they fill, so their
            // storage can reach twice their length.
            Some(Ok(fields)) => decoded_cost(fields).saturating_mul(2),
            _ => 0,
        };
        self.charge(
            request
                .len()
                .saturating_add(answer.len())
                .saturating_add(name.len())
                .saturating_add(decoded_bytes),
        )?;
        self.events.push(DryRunEvent::Request {
            request: request.to_vec(),
            service: name,
            answer: answer.clone(),
            built,
            decoded,
        });
        Ok(answer)
    }

    /// Answers `request`, the profile's `kind` of SecurityAccess request that no declared
    /// service has, and records it. A request that matches several declared services is
    /// `Ambiguous` and never reaches here.
    fn answer_profile(
        &mut self,
        request: &[u8],
        kind: ProfileRequest,
    ) -> Result<Vec<u8>, DryRunError> {
        // The positive SID and the sub-function echo come from the request. A request without
        // a sub-function is none of the profile's.
        let &[sid, sub, ..] = request else {
            return Err(DryRunError::Undeclared {
                request: error_request(request),
            });
        };
        let positive_sid = sid.wrapping_add(0x40);
        let mut built_answer = vec![positive_sid, sub];
        let script = self.script;
        let (name, scripted) = match kind {
            ProfileRequest::Seed => {
                built_answer = built_seed_response(positive_sid, sub, None);
                (
                    PROFILE_SEED_REQUEST,
                    self.next(Answers::Seed, &script.seeds),
                )
            }
            // Its scripted response is out of scope (`Script`): always built.
            ProfileRequest::Key => (PROFILE_KEY_REQUEST, None),
        };
        let built = scripted.is_none();
        let answer = scripted.unwrap_or(built_answer);
        self.charge(
            request
                .len()
                .saturating_add(answer.len())
                .saturating_add(name.len()),
        )?;
        self.events.push(DryRunEvent::Request {
            request: request.to_vec(),
            service: name.to_owned(),
            answer: answer.clone(),
            built,
            decoded: None,
        });
        Ok(answer)
    }

    fn send(&mut self, request: Vec<u8>) -> Result<Vec<u8>, DryRunError> {
        let service = resolve(*self.variant, &request)?;
        self.answer(&request, service, false)
    }
}

/// The declared service `request` belongs to: the one with the request's SID whose request
/// prefix is the longest the request starts with.
fn resolve<'v>(variant: EcuVariant<'v>, request: &[u8]) -> Result<Service<'v>, DryRunError> {
    let undeclared = || DryRunError::Undeclared {
        request: error_request(request),
    };
    let (&sid, rest) = request.split_first().ok_or_else(undeclared)?;
    let mut best: Vec<Service<'v>> = Vec::new();
    let mut best_len = 0;
    for service in variant.services().into_iter().flatten() {
        let Some(declared) = service.request() else {
            continue;
        };
        let prefix = declared.prefix().map_or(&[][..], |p| p.bytes());
        if declared.sid() != sid || !rest.starts_with(prefix) {
            continue;
        }
        if best.is_empty() || prefix.len() > best_len {
            best = vec![service];
            best_len = prefix.len();
        } else if prefix.len() == best_len {
            best.push(service);
        }
    }
    match best.as_slice() {
        [] => Err(undeclared()),
        [service] => Ok(*service),
        _ => Err(DryRunError::Ambiguous {
            request: error_request(request),
            services: best
                .iter()
                .take(MAX_ERROR_SERVICES)
                .map(error_name)
                .collect(),
        }),
    }
}

/// A positive response built from its declaration (ADR-328 item 3): the SID and prefix, then
/// the fewest bytes each field decodes from, all zero but a terminator. The first field whose
/// extent is not known, or that reads to the end of the message, ends the answer: no field
/// after it, wherever it is placed, adds bytes, so the response needs a scripted answer to
/// decode.
fn built_response(positive: &PositiveResponse<'_>, layouts: Layouts<'_>) -> Vec<u8> {
    let prefix = positive.prefix().map_or(&[][..], |p| p.bytes());
    let mut bytes = vec![positive.sid()];
    bytes.extend_from_slice(prefix);
    let start = 8 * bytes.len() as u64;
    // Where the previous field ends, in bits from the SID.
    let mut cursor = start;
    // Terminator bytes, at byte positions, to write once the length is known.
    let mut terminators: Vec<(u64, u8)> = Vec::new();
    let mut end = start;
    for field in positive.fields().into_iter().flatten() {
        let at = match field.placement() {
            Placement::AfterPrevious => {
                byte_edge(cursor).saturating_add(u64::from(field.bit_offset()))
            }
            _ => u64::from(field.bit_offset()),
        };
        let Some(built) = least_extent(&field, layouts, at) else {
            break;
        };
        if let Some((terminator, count)) = built.terminator {
            let last = built.end / 8;
            terminators.extend((last - count..last).map(|at| (at, terminator)));
        }
        end = end.max(built.end);
        if !built.followed {
            break;
        }
        cursor = built.end;
    }
    let len = usize::try_from(end.div_ceil(8))
        .map_or(crate::MAX_BYTES_LEN, |n| n.min(crate::MAX_BYTES_LEN));
    bytes.resize(len.max(bytes.len()), 0);
    for (at, terminator) in terminators {
        if let Some(byte) = usize::try_from(at).ok().and_then(|at| bytes.get_mut(at)) {
            *byte = terminator;
        }
    }
    bytes
}

/// The first byte edge at or after bit `bits`, saturating.
fn byte_edge(bits: u64) -> u64 {
    bits.div_ceil(8).saturating_mul(8)
}

fn is_string(kind: DataKind) -> bool {
    matches!(
        kind,
        DataKind::Bytes | DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2
    )
}

/// The least a field placed at bit `at` takes in a built answer.
struct Extent {
    /// Where it ends, in bits from the SID.
    end: u64,
    /// A terminator other than zero bytes, and how many bytes before `end` it fills.
    terminator: Option<(u8, u64)>,
    /// Whether a field placed after it can follow: false when the field reads to the end of
    /// the message.
    followed: bool,
}

/// The least `field` decodes from when it starts at bit `at`, with every value zero, or `None`
/// when that is not known from the declaration (a `Select`, a repeat up to a marker, or a
/// layout of no declared size).
fn least_extent(field: &Field<'_>, layouts: Layouts<'_>, at: u64) -> Option<Extent> {
    let edge = byte_edge(at);
    let sized = |bits: u64| Extent {
        end: edge.saturating_add(bits),
        terminator: None,
        followed: true,
    };
    if field.shape_type() != Shape::NONE {
        let layout_size = |id: u32| {
            layouts
                .into_iter()
                .flatten()
                .find(|l| l.id() == id)
                .map(|l| u64::from(l.byte_size()))
                .filter(|&size| size > 0)
        };
        if let Some(group) = field.shape_as_group() {
            return Some(sized(8 * layout_size(group.layout_id())?));
        }
        let repeat = field.shape_as_repeat()?;
        return match repeat.count() {
            RepeatCount::Fixed => Some(sized(
                (8 * u64::from(repeat.fixed_count()))
                    .saturating_mul(u64::from(repeat.item_byte_size())),
            )),
            // A count field built as zero gives no items.
            RepeatCount::FromField => Some(sized(0)),
            RepeatCount::UntilEndOfPdu => Some(Extent {
                followed: false,
                ..sized(0)
            }),
            _ => None,
        };
    }
    match field.length_kind() {
        LengthKind::Fixed => Some(Extent {
            end: at.saturating_add(u64::from(field.bit_length())),
            terminator: None,
            followed: true,
        }),
        // A length field built as zero gives an empty value, which only bytes and text take;
        // a number needs at least one bit, so its extent is not known.
        LengthKind::FromField if !is_string(field.kind()) => None,
        LengthKind::FromField => Some(Extent {
            end: at,
            terminator: None,
            followed: true,
        }),
        // A zero length prefix, then no value.
        LengthKind::LeadingLength => Some(Extent {
            end: byte_edge(at.saturating_add(u64::from(field.bit_length()))),
            terminator: None,
            followed: true,
        }),
        LengthKind::Terminated => {
            let min = u64::from(field.min_bytes());
            let max = u64::from(field.max_bytes());
            let unit = if field.kind() == DataKind::Unicode2 {
                2
            } else {
                1
            };
            match field.termination() {
                Termination::Zero | Termination::HexFF if max == 0 || min + unit <= max => {
                    // `min_bytes` of data, then a terminator ends the value.
                    let terminator =
                        (field.termination() == Termination::HexFF).then_some((0xFF, unit));
                    Some(Extent {
                        terminator,
                        ..sized(8 * (min + unit))
                    })
                }
                // No room for a terminator below `max_bytes`, or the value reads to the end
                // of the message up to `max_bytes`: with more bytes after it, it is that long.
                _ if max != 0 => Some(sized(8 * max)),
                // It reads to the end of the message, so nothing can follow it.
                _ => Some(Extent {
                    followed: false,
                    ..sized(8 * min)
                }),
            }
        }
        _ => None,
    }
}

/// What `fields` count toward [`MAX_REPORT_BYTES`]: each field's size, its name and its
/// value's bytes, nested fields included.
fn decoded_cost(fields: &[DecodedField]) -> usize {
    fields.iter().fold(0, |total: usize, field| {
        let value = match &field.value {
            PhysicalValue::Text(text) => text.len(),
            PhysicalValue::Bytes(bytes) => bytes.len(),
            PhysicalValue::Group(fields) | PhysicalValue::Select { fields, .. } => {
                decoded_cost(fields)
            }
            PhysicalValue::Repeat(items) => items.iter().fold(0, |sum: usize, item| {
                sum.saturating_add(size_of::<Vec<DecodedField>>())
                    .saturating_add(decoded_cost(item))
            }),
            PhysicalValue::Unsigned(_) | PhysicalValue::Signed(_) | PhysicalValue::Float(_) => 0,
        };
        total
            .saturating_add(size_of::<DecodedField>())
            .saturating_add(field.short_name.len())
            .saturating_add(value)
    })
}

/// The heap a decoded form takes in the report: its id, and per parameter the pair, its name
/// and any text, so a form of many small parameters is charged what it holds rather than its
/// compact encoding.
fn form_cost(form: &HmiForm) -> usize {
    form.params
        .iter()
        .fold(form.id.len(), |total: usize, (key, value)| {
            let text = match value {
                hmi::HmiValue::Text(text) => text.len(),
                hmi::HmiValue::Int(_) | hmi::HmiValue::Bool(_) => 0,
            };
            total
                .saturating_add(size_of::<(String, hmi::HmiValue)>())
                .saturating_add(key.len())
                .saturating_add(text)
        })
}

impl DiagHost for ScriptedHost<'_, '_> {
    type Error = DryRunError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, DryRunError> {
        // A `ServiceRequest` operand is the SID (ADR-233); a wider one is no SID any service has.
        let mut request =
            vec![u8::try_from(service).map_err(|_| DryRunError::NotASid { service })?];
        request.extend_from_slice(payload);
        let resolved = resolve(*self.variant, &request);
        // `Ambiguous` and every other error end the run, as for any request (ADR-328 item 2):
        // only a request no declared service has is answered as the profile's.
        let kind = self.marks.get(&self.pc).copied();
        if let (Err(DryRunError::Undeclared { .. }), Some(kind)) = (&resolved, kind) {
            return self.answer_profile(&request, kind);
        }
        let seed = kind == Some(ProfileRequest::Seed);
        self.answer(&request, resolved?, seed)
    }

    fn send_receive(&mut self, service: u32, request: &[u8]) -> Result<Vec<u8>, DryRunError> {
        let declared = self
            .variant
            .services()
            .into_iter()
            .flatten()
            .find(|s| s.id() == service && s.positive().is_some())
            .ok_or(DryRunError::UnknownService { service })?;
        let sid = declared
            .request()
            .ok_or(DryRunError::UnknownService { service })?
            .sid();
        if request.first() != Some(&sid) {
            return Err(DryRunError::WrongService {
                service: error_name(&declared),
                sid,
                request: error_request(request),
            });
        }
        self.answer(request, declared, false)
    }

    // The bytes the agent sends for these primitives (`agent::host`): ReadDTCInformation with
    // reportDTCByStatusMask, and RoutineControl with the routine identifier big-endian.
    fn read_dtc(&mut self, mask: u8) -> Result<Vec<u8>, DryRunError> {
        self.send(vec![0x19, 0x02, mask])
    }

    fn routine_control(
        &mut self,
        routine: u16,
        sub: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, DryRunError> {
        let mut request = vec![0x31, sub];
        request.extend_from_slice(&routine.to_be_bytes());
        request.extend_from_slice(payload);
        self.send(request)
    }

    fn security_access(
        &mut self,
        _inquiry: u64,
        level: u8,
        seed: &[u8],
    ) -> Result<Option<Vec<u8>>, DryRunError> {
        let key = {
            let script = self.script;
            self.next(Answers::Key, &script.keys)
                .ok_or(DryRunError::NotScripted { what: "key" })?
        };
        self.charge(seed.len() + key.len())?;
        self.events.push(DryRunEvent::SecurityAccess {
            level,
            seed: seed.to_vec(),
            key: key.clone(),
        });
        Ok(Some(key))
    }

    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), DryRunError> {
        self.charge(0)?;
        self.events.push(DryRunEvent::FlashTransfer {
            block,
            len: data.len() as u64,
            digest: block_digest(data),
        });
        Ok(())
    }

    fn wait(&mut self, _inquiry: u64, millis: u32) -> Result<bool, DryRunError> {
        self.charge(0)?;
        self.events.push(DryRunEvent::Wait { millis });
        Ok(true)
    }

    fn hmi_request(&mut self, _inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, DryRunError> {
        let answer = {
            let script = self.script;
            self.next(Answers::Form, &script.forms)
                .ok_or(DryRunError::NotScripted { what: "HMI answer" })?
        };
        // Decoding is bounded by the form's 16 KiB (ADR-340 item 1); what the report keeps is
        // the decoded form.
        let form = hmi::decode_form(form).map_err(|_| DryRunError::BadHmiForm)?;
        self.charge(form_cost(&form).saturating_add(answer.len()))?;
        self.events.push(DryRunEvent::Hmi {
            form,
            answer: answer.clone(),
        });
        Ok(Some(answer))
    }

    fn record_input(
        &mut self,
        _inquiry: u64,
        template: &[u8],
    ) -> Result<Option<Vec<u8>>, DryRunError> {
        let answer = {
            let script = self.script;
            self.next(Answers::Record, &script.records)
                .ok_or(DryRunError::NotScripted {
                    what: "record answer",
                })?
        };
        let template = hmi::decode_form(template).map_err(|_| DryRunError::BadHmiForm)?;
        self.charge(form_cost(&template).saturating_add(answer.len()))?;
        self.events.push(DryRunEvent::RecordInput {
            template,
            answer: answer.clone(),
        });
        Ok(Some(answer))
    }

    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), DryRunError> {
        self.charge(0)?;
        self.events
            .push(DryRunEvent::MonitorCapture { back_millis });
        Ok(())
    }

    // A dry run has no artifact: the block is zero bytes of the requested length (ADR-336 item
    // 15). It leaves no event and the report keeps none of its bytes (`flash_transfer` records
    // a length and a digest), so it is not charged: an image larger than the report's budget
    // is a valid plan (ADR-391). One block is at most `MAX_BYTES_LEN` (`check_declarations`),
    // and the VM's stack bounds how many it holds at once.
    fn artifact_bytes(&mut self, _offset: u64, len: u32) -> Result<Vec<u8>, DryRunError> {
        Ok(vec![0; len as usize])
    }

    // A read of the script's value for the input: it sends nothing and allocates nothing, so it
    // leaves no event and charges nothing (ADR-338).
    fn runtime_input(&mut self, input: RuntimeInput) -> Result<i64, DryRunError> {
        self.script
            .inputs
            .iter()
            .find(|(set, _)| *set == input)
            .map(|&(_, value)| value)
            .ok_or(DryRunError::NotScripted {
                what: "value for the runtime input",
            })
    }

    // `log` cannot fail, so a message past the budget is dropped and the run ends after this
    // step (`ScriptedHost::over_budget`).
    fn log(&mut self, level: u8, message: &str) {
        if self.charge(message.len()).is_err() {
            return;
        }
        self.events.push(DryRunEvent::Log {
            level,
            message: message.to_owned(),
        });
    }
}

#[cfg(test)]
#[path = "dry_run_tests.rs"]
mod tests;
