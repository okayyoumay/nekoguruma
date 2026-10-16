//! Application-layer profiles (ADR-309 item 2, ADR-314): how the protocol-neutral Starlark
//! calls `diag.read_dtc`, `diag.routine` and `diag.security_access` expand to one application
//! layer's services, and how those services' responses map to the call's result.
//!
//! A profile is typed data: a `static` [`Profile`] built from `&'static` slices, checked by
//! [`Profile::validate`] in tests. It ships with the frontend, so a profile that breaks a rule
//! is a programming error, not an ingestion error. A call's request is a template of [`Part`]s
//! that select the variant's declared service by value and position (`docs/csv-tables.md` 5.3,
//! ADR-313), and its result names are mapped to positions among the declared response's
//! top-level fields, so a definition keeps its own field names.

/// The profiles that ship with the frontend, one per application layer.
pub const ALL: &[&Profile] = &[];

/// The profile of the application layer `layer` (`Profile::layer`), if one ships.
pub fn get(layer: &str) -> Option<&'static Profile> {
    ALL.iter().copied().find(|profile| profile.layer == layer)
}

/// The response-pending codes a profile may use: those the workers absorb (ADR-309 item 5).
pub const PENDING_CODES: [u8; 3] = [0x78, 0x21, 0x23];

/// One application layer's expansion of the protocol-neutral calls.
#[derive(Debug)]
pub struct Profile {
    /// The application layer's name, as a protocol of a variant names it.
    pub layer: &'static str,
    /// The calls the layer defines, each intent at most once.
    pub calls: &'static [Call],
    /// The shape of a negative response.
    pub negative: NegativeShape,
    /// The negative-response codes that mean the answer is still pending.
    pub pending: &'static [u8],
}

/// Where a negative response carries what (ADR-309 item 5): a marker byte first, then the
/// request's service identifier if `echoes_sid`, and the code at byte `code_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegativeShape {
    pub marker: u8,
    pub echoes_sid: bool,
    pub code_at: u8,
}

/// The one negative-response shape the workers recognise: `7F`, the service identifier, the
/// code (ADR-309 item 5).
pub const NEGATIVE_7F: NegativeShape = NegativeShape {
    marker: 0x7F,
    echoes_sid: true,
    code_at: 2,
};

/// A protocol-neutral call (`docs/starlark-subset.md` 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// `diag.read_dtc(mask)`.
    ReadDtc,
    /// `diag.routine(routine_id, sub, params)`.
    Routine,
    /// `diag.security_access(level)`.
    SecurityAccess,
}

impl Intent {
    /// The call's name in the procedure language.
    pub fn name(self) -> &'static str {
        match self {
            Self::ReadDtc => "diag.read_dtc",
            Self::Routine => "diag.routine",
            Self::SecurityAccess => "diag.security_access",
        }
    }
}

/// How one intent expands: its steps, run in order.
#[derive(Debug)]
pub struct Call {
    pub intent: Intent,
    pub steps: &'static [Step],
}

/// One message exchange of a call, or the host's seed-to-key step.
#[derive(Debug)]
pub struct Step {
    pub kind: StepKind,
    /// The request; empty for a [`StepKind::SecurityKey`] step, which sends nothing.
    pub request: Request,
    /// Whether the response is decoded through a service the variant declares, selected by
    /// `request`; otherwise only the profile's own rules read it.
    pub declared: bool,
    /// How the response maps to the call's result.
    pub result: ResultMap,
}

/// Which instruction a step lowers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// The generic send-receive (ADR-309 item 4).
    SendReceive,
    /// `RoutineControl`, kept on UDS since recovery relies on it (ADR-309 items 4 and 6). It
    /// sends the sub-function and the 16-bit routine identifier itself.
    RoutineControl,
    /// `SecurityAccess`: the server turns the seed of the previous step into a key.
    SecurityKey,
}

/// A request template: the service identifier, then its parts in wire order.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub sid: u8,
    pub parts: &'static [Part],
}

/// One part of a request template, after the service identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// A constant the profile fixes; the declared service must hold a constant of this value
    /// here.
    Const { bits: u8, value: u64 },
    /// A call argument that selects the declared service: it must equal the declared constant
    /// here.
    Select { bits: u8, arg: Source },
    /// A call argument encoded into the declared field here; `bits` 0 means the rest of the
    /// request.
    Arg { bits: u8, arg: Source },
    /// The call's `params`: their keys are exactly the remaining declared request fields, as
    /// for `diag.request` (`docs/csv-tables.md` 5.3).
    Params,
}

/// Where a part's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `mask` of `diag.read_dtc`.
    Mask,
    /// `routine_id` of `diag.routine`.
    Rid,
    /// `sub` of `diag.routine`.
    Sub,
    /// `level` of `diag.security_access`.
    Level,
    /// `level + 1`: the key request's sub-function.
    LevelPlusOne,
    /// The key the [`StepKind::SecurityKey`] step returns.
    Key,
}

impl Source {
    /// The intent whose arguments the source is.
    fn intent(self) -> Intent {
        match self {
            Self::Mask => Intent::ReadDtc,
            Self::Rid | Self::Sub => Intent::Routine,
            Self::Level | Self::LevelPlusOne | Self::Key => Intent::SecurityAccess,
        }
    }
}

/// The index of a field among the declared response's top-level fields after its constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos(pub u8);

/// How a step's response maps to the call's result (`docs/starlark-subset.md` 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultMap {
    /// `diag.read_dtc`: `mask` is the field at `mask`; `dtcs` the repeat-until-end field at
    /// `dtcs`, whose records are a `code_bits`-bit code then a `status_bits`-bit status.
    ReadDtc {
        mask: Pos,
        dtcs: Pos,
        code_bits: u8,
        status_bits: u8,
    },
    /// The declared response's own field names, as for `diag.request`.
    Declared,
    /// The seed: the response bytes after the first `after_bytes`.
    Seed { after_bytes: u8 },
    /// Nothing; the call's `raw` is the response.
    None,
}

/// A rule of ADR-314 a profile breaks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("the negative response is not `7F`, the service identifier and the code")]
    NegativeShape,
    #[error("pending code 0x{0:02X} is not one the workers absorb (0x78, 0x21, 0x23)")]
    PendingCode(u8),
    #[error("pending code 0x{0:02X} is listed twice")]
    PendingTwice(u8),
    #[error("{} is defined twice", .0.name())]
    IntentTwice(Intent),
    #[error("{} has no step", .0.name())]
    NoStep(Intent),
    #[error("{}: a constant is not whole bytes or does not fit its width", .0.name())]
    BadConstant(Intent),
    #[error("{}: an argument's width is not whole bytes", .0.name())]
    BadWidth(Intent),
    #[error("{}: `params` is not the last part, or appears twice", .0.name())]
    ParamsNotLast(Intent),
    #[error("{}: {:?} is not an argument of this call", .0.name(), .1)]
    ForeignSource(Intent, Source),
    #[error("{}: {:?} is used {} times, not once", .0.name(), .1, .2)]
    SourceCount(Intent, Source, usize),
    #[error("{}: `params` is used {} times, not once", .0.name(), .1)]
    ParamsCount(Intent, usize),
    #[error("{}: the key is used before the step that makes it", .0.name())]
    KeyBeforeKeyStep(Intent),
    #[error("{}: a step's kind does not fit the call", .0.name())]
    StepKind(Intent),
    #[error("{}: a step's result map does not fit the call", .0.name())]
    ResultMap(Intent),
}

impl Profile {
    /// The steps of `intent`, if the profile defines it.
    pub fn call(&self, intent: Intent) -> Option<&Call> {
        self.calls.iter().find(|call| call.intent == intent)
    }

    /// Checks the rules of ADR-314: the negative-response shape and pending codes the workers
    /// support (ADR-309 item 5), and calls whose steps can be lowered.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.negative != NEGATIVE_7F {
            return Err(ProfileError::NegativeShape);
        }
        for (i, &code) in self.pending.iter().enumerate() {
            if !PENDING_CODES.contains(&code) {
                return Err(ProfileError::PendingCode(code));
            }
            if self.pending[..i].contains(&code) {
                return Err(ProfileError::PendingTwice(code));
            }
        }
        for (i, call) in self.calls.iter().enumerate() {
            if self.calls[..i].iter().any(|c| c.intent == call.intent) {
                return Err(ProfileError::IntentTwice(call.intent));
            }
            call.validate()?;
        }
        Ok(())
    }
}

impl Call {
    fn validate(&self) -> Result<(), ProfileError> {
        let intent = self.intent;
        if self.steps.is_empty() {
            return Err(ProfileError::NoStep(intent));
        }
        let mut key_made = false;
        let mut params = 0;
        let mut used: Vec<Source> = Vec::new();
        for step in self.steps {
            let kind_fits = match step.kind {
                StepKind::SendReceive => true,
                StepKind::RoutineControl => {
                    intent == Intent::Routine
                        && matches!(
                            step.request.parts,
                            [
                                Part::Select {
                                    bits: 8,
                                    arg: Source::Sub
                                },
                                Part::Select {
                                    bits: 16,
                                    arg: Source::Rid
                                },
                                ..
                            ]
                        )
                }
                StepKind::SecurityKey => {
                    intent == Intent::SecurityAccess
                        && !key_made
                        && step.request.parts.is_empty()
                        && !step.declared
                }
            };
            if !kind_fits {
                return Err(ProfileError::StepKind(intent));
            }
            let map_fits = match step.result {
                ResultMap::ReadDtc {
                    code_bits,
                    status_bits,
                    ..
                } => {
                    intent == Intent::ReadDtc
                        && step.declared
                        && code_bits > 0
                        && code_bits % 8 == 0
                        && status_bits > 0
                        && status_bits % 8 == 0
                }
                ResultMap::Declared => step.declared,
                ResultMap::Seed { .. } => intent == Intent::SecurityAccess,
                ResultMap::None => true,
            };
            if !map_fits {
                return Err(ProfileError::ResultMap(intent));
            }
            let parts = step.request.parts;
            for (i, part) in parts.iter().enumerate() {
                match *part {
                    Part::Const { bits, value } => {
                        if bits == 0 || bits % 8 != 0 || (bits < 64 && value >> bits != 0) {
                            return Err(ProfileError::BadConstant(intent));
                        }
                    }
                    Part::Select { bits, arg } | Part::Arg { bits, arg } => {
                        let rest = matches!(part, Part::Arg { bits: 0, .. });
                        if bits % 8 != 0 || (bits == 0 && !rest) || bits > 64 {
                            return Err(ProfileError::BadWidth(intent));
                        }
                        if arg.intent() != intent {
                            return Err(ProfileError::ForeignSource(intent, arg));
                        }
                        if arg == Source::Key && !key_made {
                            return Err(ProfileError::KeyBeforeKeyStep(intent));
                        }
                        used.push(arg);
                    }
                    Part::Params => {
                        params += 1;
                        if i + 1 != parts.len() {
                            return Err(ProfileError::ParamsNotLast(intent));
                        }
                    }
                }
            }
            if step.kind == StepKind::SecurityKey {
                key_made = true;
            }
        }
        let (required, needs_params): (&[Source], usize) = match intent {
            Intent::ReadDtc => (&[Source::Mask], 0),
            Intent::Routine => (&[Source::Sub, Source::Rid], 1),
            Intent::SecurityAccess => (&[Source::Level, Source::LevelPlusOne, Source::Key], 0),
        };
        for &source in required {
            let count = used.iter().filter(|&&s| s == source).count();
            if count != 1 {
                return Err(ProfileError::SourceCount(intent, source, count));
            }
        }
        if params != needs_params {
            return Err(ProfileError::ParamsCount(intent, params));
        }
        if intent == Intent::SecurityAccess && !key_made {
            return Err(ProfileError::StepKind(intent));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
