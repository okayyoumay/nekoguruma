//! Dry run of a procedure with vehicle access mocked (design 12.1, ADR-328).
//!
//! [`dry_run`] steps a [`Program`] with the declaration part of its variant against a host that
//! sends nothing. Each request is resolved to the declared service it belongs to, answered from
//! a [`Script`] or, without a scripted answer, with a positive response built from the
//! service's declaration, and decoded through that declaration. The [`DryRunReport`] lists the
//! requests with their answers and decoded values, the other primitives the procedure reached,
//! and how the run ended.

use std::collections::HashMap;

use crate::declaration::{EcuVariant, LengthKind, Placement, PositiveResponse, Service};
use crate::decode::{DecodeError, DecodedField, decode_positive_response};
use crate::{
    DiagHost, Failure, Program, ProgramError, StepError, StepOutcome, Vm, VmError, WaitingOn,
};

/// The answers a dry run gives. Each list is answered in order, and its last answer again once
/// the list is used up, so a procedure that polls gets a steady answer.
#[derive(Debug, Clone, Default)]
pub struct Script {
    responses: HashMap<String, Vec<Vec<u8>>>,
    keys: Vec<Vec<u8>>,
    forms: Vec<Vec<u8>>,
    records: Vec<Vec<u8>>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a whole response, starting with its SID, to the answers to the service named
    /// `service` (`Service.short_name`). A negative response may be scripted too.
    pub fn response(mut self, service: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.responses
            .entry(service.to_owned())
            .or_default()
            .push(bytes.into());
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
}

/// What the procedure did, in order.
#[derive(Debug, PartialEq)]
pub enum DryRunEvent {
    /// A request sent by `ServiceRequest`, `SendReceive`, `ReadDtc` or `RoutineControl`, as the
    /// whole request starting with its SID.
    Request {
        request: Vec<u8>,
        /// `Service.short_name` of the declared service the request belongs to.
        service: String,
        answer: Vec<u8>,
        /// Whether the answer was built from the declaration rather than scripted.
        built: bool,
        /// The answer decoded through the service's positive response; `None` when the service
        /// declares none.
        decoded: Option<Result<Vec<DecodedField>, DecodeError>>,
    },
    SecurityAccess {
        level: u8,
        seed: Vec<u8>,
        key: Vec<u8>,
    },
    FlashTransfer {
        block: u32,
        data: Vec<u8>,
    },
    /// Passes at once: a dry run does not wait.
    Wait {
        millis: u32,
    },
    Hmi {
        form: Vec<u8>,
        answer: Vec<u8>,
    },
    RecordInput {
        template: Vec<u8>,
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

/// Why the mocked vehicle access could not answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DryRunError {
    /// No declared service has the request's SID and a prefix the request starts with.
    #[error("the request {request:02X?} belongs to no declared service")]
    Undeclared { request: Vec<u8> },
    /// Several declared services match the request equally well.
    #[error("the request {request:02X?} matches several declared services: {services:?}")]
    Ambiguous {
        request: Vec<u8>,
        services: Vec<String>,
    },
    /// A `SendReceive` names a service the variant does not declare.
    #[error("no declared service has id {service}")]
    UnknownService { service: u32 },
    /// The service has no declared positive response and no answer is scripted.
    #[error("service {service} has no positive response to build an answer from")]
    NoPositiveResponse { service: String },
    /// The script has no answer of this kind.
    #[error("the script has no {what}")]
    NotScripted { what: &'static str },
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
        events: Vec::new(),
    };
    if let Err(error) = program.validate() {
        return DryRunReport {
            events: Vec::new(),
            end: DryRunEnd::Invalid(error),
            steps: 0,
        };
    }
    let mut vm = Vm::new(program);
    let end = loop {
        if vm.state.steps >= max_steps {
            break DryRunEnd::StepLimit;
        }
        match vm.step_with_declarations(program, Some(variant), &mut host) {
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
    }
}

/// Which list of the script an answer comes from, for the count of answers used.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Answers {
    Response(String),
    Key,
    Form,
    Record,
}

struct ScriptedHost<'s, 'v> {
    variant: &'s EcuVariant<'v>,
    script: &'s Script,
    used: HashMap<Answers, usize>,
    events: Vec<DryRunEvent>,
}

impl ScriptedHost<'_, '_> {
    /// The next answer from `list`, or its last once it is used up.
    fn next(&mut self, which: Answers, list: &[Vec<u8>]) -> Option<Vec<u8>> {
        let last = list.len().checked_sub(1)?;
        let used = self.used.entry(which).or_default();
        let answer = list[(*used).min(last)].clone();
        *used += 1;
        Some(answer)
    }

    /// Answers `request` as `service`, records it, and returns the answer.
    fn answer(&mut self, request: &[u8], service: Service<'_>) -> Result<Vec<u8>, DryRunError> {
        let name = service.short_name().unwrap_or_default().to_owned();
        let script = self.script;
        let scripted = script.responses.get(&name).map_or(&[][..], Vec::as_slice);
        let (answer, built) = match self.next(Answers::Response(name.clone()), scripted) {
            Some(answer) => (answer, false),
            None => {
                let positive =
                    service
                        .positive()
                        .ok_or_else(|| DryRunError::NoPositiveResponse {
                            service: name.clone(),
                        })?;
                (built_response(&positive), true)
            }
        };
        let decoded = service
            .positive()
            .map(|positive| decode_positive_response(&positive, self.variant.layouts(), &answer));
        self.events.push(DryRunEvent::Request {
            request: request.to_vec(),
            service: name,
            answer: answer.clone(),
            built,
            decoded,
        });
        Ok(answer)
    }

    fn send(&mut self, request: Vec<u8>) -> Result<Vec<u8>, DryRunError> {
        let service = resolve(*self.variant, &request)?;
        self.answer(&request, service)
    }
}

/// The declared service `request` belongs to: the one with the request's SID whose request
/// prefix is the longest the request starts with.
fn resolve<'v>(variant: EcuVariant<'v>, request: &[u8]) -> Result<Service<'v>, DryRunError> {
    let undeclared = || DryRunError::Undeclared {
        request: request.to_vec(),
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
            request: request.to_vec(),
            services: best
                .iter()
                .map(|s| s.short_name().unwrap_or_default().to_owned())
                .collect(),
        }),
    }
}

/// A positive response built from its declaration: the SID and prefix, then zero bytes up to
/// the end of the last field whose position and length are fixed. A field of another length,
/// or one with a shape, takes no bytes, nor does any field placed after it.
fn built_response(positive: &PositiveResponse<'_>) -> Vec<u8> {
    let prefix = positive.prefix().map_or(&[][..], |p| p.bytes());
    let start = 8 * (1 + prefix.len() as u64);
    // Where the previous field ends, in bits from the SID; `None` once it is not known.
    let mut cursor = Some(start);
    let mut end = start;
    for field in positive.fields().into_iter().flatten() {
        let at = match field.placement() {
            Placement::AfterPrevious => match cursor {
                Some(previous) => previous.div_ceil(8) * 8 + u64::from(field.bit_offset()),
                None => break,
            },
            _ => u64::from(field.bit_offset()),
        };
        if field.length_kind() != LengthKind::Fixed || field.shape().is_some() {
            cursor = None;
            continue;
        }
        let field_end = at + u64::from(field.bit_length());
        cursor = Some(field_end);
        end = end.max(field_end);
    }
    let len = usize::try_from(end.div_ceil(8))
        .map_or(crate::MAX_BYTES_LEN, |n| n.min(crate::MAX_BYTES_LEN));
    let mut bytes = vec![positive.sid()];
    bytes.extend_from_slice(prefix);
    bytes.resize(len.max(bytes.len()), 0);
    bytes
}

impl DiagHost for ScriptedHost<'_, '_> {
    type Error = DryRunError;

    fn service_request(&mut self, service: u16, payload: &[u8]) -> Result<Vec<u8>, DryRunError> {
        // A `ServiceRequest` operand is the SID (ADR-233); a wider one is no SID any service has.
        let mut request = vec![u8::try_from(service).map_err(|_| DryRunError::Undeclared {
            request: payload.to_vec(),
        })?];
        request.extend_from_slice(payload);
        self.send(request)
    }

    fn send_receive(&mut self, service: u32, request: &[u8]) -> Result<Vec<u8>, DryRunError> {
        let declared = self
            .variant
            .services()
            .into_iter()
            .flatten()
            .find(|s| s.id() == service)
            .ok_or(DryRunError::UnknownService { service })?;
        self.answer(request, declared)
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
        self.events.push(DryRunEvent::SecurityAccess {
            level,
            seed: seed.to_vec(),
            key: key.clone(),
        });
        Ok(Some(key))
    }

    fn flash_transfer(&mut self, block: u32, data: &[u8]) -> Result<(), DryRunError> {
        self.events.push(DryRunEvent::FlashTransfer {
            block,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn wait(&mut self, _inquiry: u64, millis: u32) -> Result<bool, DryRunError> {
        self.events.push(DryRunEvent::Wait { millis });
        Ok(true)
    }

    fn hmi_request(&mut self, _inquiry: u64, form: &[u8]) -> Result<Option<Vec<u8>>, DryRunError> {
        let answer = {
            let script = self.script;
            self.next(Answers::Form, &script.forms)
                .ok_or(DryRunError::NotScripted { what: "HMI answer" })?
        };
        self.events.push(DryRunEvent::Hmi {
            form: form.to_vec(),
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
        self.events.push(DryRunEvent::RecordInput {
            template: template.to_vec(),
            answer: answer.clone(),
        });
        Ok(Some(answer))
    }

    fn monitor_capture(&mut self, back_millis: u32) -> Result<(), DryRunError> {
        self.events
            .push(DryRunEvent::MonitorCapture { back_millis });
        Ok(())
    }

    fn log(&mut self, level: u8, message: &str) {
        self.events.push(DryRunEvent::Log {
            level,
            message: message.to_owned(),
        });
    }
}
