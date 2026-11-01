//! The identification job's plan: loading it with its documents, the checks before any link
//! opens, and the endpoints and presence groups derived from the declared links (design 8.8.2,
//! ADR-327, ADR-395).
//!
//! [`load_plan`] and [`check_plan`] send nothing and are the only gate before a link opens, and
//! [`CheckedPlan::report`] is the only writer of the report's endpoint and presence-group lists.
//! [`run_identification`] then sends the plan's functional presence requests and is the only
//! writer of the report's presence section (ADR-396); it then reads the VIN the plan's VIN rules
//! name, and is the only writer of the report's VIN section and of the per-vehicle lock the
//! identified VIN takes (ADR-399). It then reads the ECU definitions the plan names, once each, in
//! the order ADR-327 item 4 gives, and is the only writer of the report's ECU reads and entry
//! states (ADR-401); a plan whose selection narrows drops the candidates the reads and the VIN
//! rules contradict, and reads no definition that no remaining candidate names (ADR-402). The
//! fields the candidates' matching rules read are requests of the definitions' reads: their
//! values are established per component when their sources agree, and a candidate whose rules on
//! an established field none matched is dropped (ADR-403).

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diag_ir::declaration::{DataKind, EcuVariant, LengthKind};
use diag_ir::decode::{
    DecodedField, ExpectedKind, PhysicalValue, expected_matches, expected_value, variant_matches,
};
use diag_ir::identify::{
    IdentifyError, decode_identification_answer, identification_request_of, identification_requests,
};
use diag_ir::plan::{
    Component, Entry as PlanEntry, FieldRule, IdentificationPlan, PlanError, Selection, Sha256,
    Source, VinRule, hex,
};
use diag_ir::{DiagHost, IdentityKind};
use serde::{Serialize, Serializer};
use sha2::Digest;
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::client::{WorkerApi, WorkerExit, WorkerSource, WorkerStatus};
use crate::comm_log::{
    CommLogSetup, CommLogStatus, JobKind, JobLog, RunFacts, RunKind, VciFacts, VciMargins,
};
use crate::document::{DocumentError, JobDocument};
use crate::guards::JobGuards;
use crate::host::{
    HostError, OwnRead, ProcedurePosition, ReopenAttempt, ResendClass, Timings, WorkerHost,
    names_lost_vci, settle_worker,
};
use crate::inputs::{
    FieldBytes, ServiceField, ServiceSources, field_bytes_of, identity_read_of, negative_code,
    vin_source_field,
};
use crate::journal::Vin;
use crate::link::{BUS_TYPE_DWCAN, CloseEvidence, LinkConfig, MAX_CAN_ID};
use crate::policy::{ApplicationLayer, PRESENCE_REQUEST};
use crate::runner::{
    CancelOnDrop, GuardSlot, JobError, JobLimits, JobLink, LinkRefused, WORKER_EXIT_GRACE,
    base_facts, close_job_link, on_job_thread, open_job_link, promote_to_vehicle, reopen_attempts,
};
use crate::send_receive::declared_sid;

/// Why a plan is refused before any link opens.
#[derive(Debug, thiserror::Error)]
pub enum PlanRefused {
    #[error("{0}")]
    Plan(#[from] PlanError),
    #[error("the plan names document {0}, which was not supplied")]
    MissingDocument(String),
    #[error("a document supplied as {named} has the SHA-256 {actual}")]
    HashMismatch { named: String, actual: String },
    #[error("document {document} cannot be used: {error}")]
    BadDocument {
        document: String,
        error: DocumentError,
    },
    /// A service that a variant pattern or a rule source names is not in the read-only set of
    /// its application layer, or the document does not declare it (ADR-327 item 1).
    #[error(
        "document {document} names service {service}, which is not a declared read-only service"
    )]
    NotReadOnly { document: String, service: u32 },
    #[error("document {document} has no field {field} in the response of service {service}")]
    UnknownField {
        document: String,
        service: u32,
        field: u32,
    },
    /// Declared links in one presence group (a bus and a functional identifier) that differ in
    /// a ComParam other than a timing (ADR-395 item 6).
    #[error(
        "documents {first} and {second} are in one presence group and declare different ComParams"
    )]
    ComParamsDiffer { first: String, second: String },
    /// One resource (the same protocol, bus type and pins) declared at two baud rates.
    #[error("documents {first} and {second} declare one resource at two baud rates")]
    BaudRatesDiffer { first: String, second: String },
    /// A VIN rule's source that the identification job cannot read as a VIN: the document does
    /// not declare it in the shape of the ADR-345 table, or it is not 17 bytes of ASCII text
    /// (ADR-399 item 2).
    #[error("document {document} cannot give a VIN from service {service} field {field}: {why}")]
    VinSource {
        document: String,
        service: u32,
        field: u32,
        why: String,
    },
    /// A service a variant pattern names whose identification request cannot be built without
    /// values (ADR-401): known before any link opens.
    #[error(
        "document {document} cannot build the identification request of service {service}: {why}"
    )]
    IdentificationRequest {
        document: String,
        service: u32,
        why: String,
    },
    /// A field rule's source that the identification job cannot judge: its request needs values,
    /// its field is one a variant pattern could not compare (ADR-306 item 5), its expected value
    /// could never match, or the sources of one component on one request decode to different
    /// kinds (ADR-403).
    #[error(
        "document {document} cannot give a field rule value from service {service} field {field}: {why}"
    )]
    FieldSource {
        document: String,
        service: u32,
        field: u32,
        why: String,
    },
    #[error("document {document} declares functional identifier {functional_id:#x}: {why}")]
    BadFunctionalId {
        document: String,
        functional_id: u32,
        why: &'static str,
    },
}

/// The bus a link is on: the resource the agent resolves for it (protocol short name and bus
/// type, ADR-319) with its baud rate, so two spellings of one stack that ADR-324 item 2
/// accepts name the same bus. The DLC pins follow from the bus type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BusKey {
    pub protocol: String,
    pub bus_type: String,
    pub baud_rate: u32,
}

impl BusKey {
    pub fn of(link: &LinkConfig) -> Self {
        Self {
            protocol: link.protocol_short_name.clone(),
            bus_type: BUS_TYPE_DWCAN.to_owned(),
            baud_rate: link.baud_rate,
        }
    }
}

/// An ECU's physical address: its bus and the request and response identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndpointKey {
    pub bus: BusKey,
    pub tx_id: u32,
    pub rx_id: u32,
}

/// An endpoint and the documents whose declared link is on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndpointEntry {
    pub key: EndpointKey,
    #[serde(serialize_with = "hashes_as_hex")]
    pub documents: Vec<Sha256>,
}

/// Bytes (a hash among them) as the lowercase hex a report shows them in.
fn as_hex<T: AsRef<[u8]>, S: Serializer>(bytes: &T, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&hex(bytes.as_ref()))
}

fn hashes_as_hex<S: Serializer>(hashes: &[Sha256], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(hashes.iter().map(|hash| hex(hash)))
}

/// The links that send one functional presence request together: a bus and a functional
/// identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresenceGroup {
    pub bus: BusKey,
    pub functional_id: u32,
    /// `CP_P2Max` of the presence link, in milliseconds: the longest over the declared links of
    /// the group (ADR-395 item 5).
    pub p2_max_ms: u32,
    /// `CP_P2Star` and `CP_RC78CompletionTimeout`, in milliseconds: the largest of the group.
    pub p2_star_ms: u32,
    pub rc78_completion_ms: u32,
    /// The other timing ComParams the group's links declare (`TIMING_PARAMS`), by lowercase
    /// name in a fixed order, each with its largest value in the ComParam's own unit
    /// (ADR-395 item 6).
    pub timings: Vec<(String, u32)>,
}

impl PresenceGroup {
    /// Raises each timing of the group to `link`'s when that is larger (ADR-395 item 6), by
    /// [`raise_timings`] on a copy of `link` that carries the group's timings.
    fn take_largest(&mut self, link: &LinkConfig) {
        let mut raised = link.clone();
        self.write_timings(&mut raised);
        raise_timings(&mut raised, link);
        self.p2_max_ms = raised.p2_max_ms;
        self.p2_star_ms = raised.p2_star_ms;
        self.rc78_completion_ms = raised.rc78_completion_ms;
        self.timings = raised
            .params
            .iter()
            .filter(|(name, _)| is_timing(name))
            .map(|(name, value)| (name.to_ascii_lowercase(), *value))
            .collect();
        self.timings.sort();
    }

    /// Sets the group's timings on `link`, replacing its own.
    fn write_timings(&self, link: &mut LinkConfig) {
        link.p2_max_ms = self.p2_max_ms;
        link.p2_star_ms = self.p2_star_ms;
        link.rc78_completion_ms = self.rc78_completion_ms;
        for (name, value) in &self.timings {
            match link
                .params
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                Some((_, known)) => *known = *value,
                None => link.params.push((name.clone(), *value)),
            }
        }
    }
}

/// Raises each timing of `target` to `link`'s when that is larger: a larger value only makes the
/// tester wait longer or send later (ADR-395 item 6). A presence group takes the largest of its
/// links so, and a VIN read does (ADR-399 item 1).
fn raise_timings(target: &mut LinkConfig, link: &LinkConfig) {
    target.p2_max_ms = target.p2_max_ms.max(link.p2_max_ms);
    target.p2_star_ms = target.p2_star_ms.max(link.p2_star_ms);
    target.rc78_completion_ms = target.rc78_completion_ms.max(link.rc78_completion_ms);
    for (name, value) in link.params.iter().filter(|(name, _)| is_timing(name)) {
        match target
            .params
            .iter_mut()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            Some((_, largest)) => *largest = (*largest).max(*value),
            None => target.params.push((name.clone(), *value)),
        }
    }
}

/// A presence answer: the bus and the response identifier that answered (ADR-327 item 2), with
/// the bytes it carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresenceAnswer {
    pub bus: BusKey,
    pub rx_id: u32,
    #[serde(serialize_with = "as_hex")]
    pub bytes: Vec<u8>,
}

/// A receive table the VCI refused, which the agent then halved (ADR-396).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedTable {
    pub bus: BusKey,
    pub functional_id: u32,
    /// The physical request and response identifier of each pair of the table.
    pub table: Vec<(u32, u32)>,
}

/// A document whose declared link ADR-324 refuses: the job cannot read it, which leaves its
/// definition inconclusive at read time (ADR-327 item 3), and the plan stays valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusedLink {
    #[serde(serialize_with = "as_hex")]
    pub document: Sha256,
    pub reason: String,
}

/// A plan and the documents it names, each checked against its hash.
#[derive(Debug)]
pub struct LoadedPlan {
    plan: IdentificationPlan,
    plan_sha256: Sha256,
    documents: Vec<(Sha256, JobDocument)>,
}

/// Reads a plan and the documents it names. `supplied` holds the documents with the hash each
/// is named by; one the plan names must be present and hash to its name, and a supplied
/// document the plan does not name is left unread.
pub fn load_plan(
    plan_bytes: &[u8],
    supplied: Vec<(Sha256, Vec<u8>)>,
) -> Result<LoadedPlan, PlanRefused> {
    let plan = IdentificationPlan::decode(plan_bytes)?;
    let mut documents = Vec::new();
    for named in plan.document_hashes() {
        let (_, bytes) = supplied
            .iter()
            .find(|(hash, _)| *hash == named)
            .ok_or_else(|| PlanRefused::MissingDocument(hex(&named)))?;
        let actual: Sha256 = sha2::Sha256::digest(bytes).into();
        if actual != named {
            return Err(PlanRefused::HashMismatch {
                named: hex(&named),
                actual: hex(&actual),
            });
        }
        let document =
            JobDocument::new(bytes.clone()).map_err(|error| PlanRefused::BadDocument {
                document: hex(&named),
                error,
            })?;
        documents.push((named, document));
    }
    Ok(LoadedPlan {
        plan,
        plan_sha256: sha2::Sha256::digest(plan_bytes).into(),
        documents,
    })
}

impl LoadedPlan {
    /// The hashes of the documents the plan names, which [`load_plan`] found among those
    /// supplied.
    pub fn document_hashes(&self) -> Vec<Sha256> {
        self.documents.iter().map(|(hash, _)| *hash).collect()
    }
}

/// A declared link, with the functional identifier its protocol declares (0 when none).
#[derive(Debug)]
struct DocumentLink {
    document: Sha256,
    link: LinkConfig,
    functional_id: u32,
}

/// One read of the VIN: the sources that one request on one link answers (ADR-399 item 1).
#[derive(Debug)]
struct VinReadPlan {
    /// The endpoint the read goes to; `None` for sources whose document's link ADR-324 refuses.
    endpoint: Option<EndpointKey>,
    /// The first declared link of the read with the largest timings of its sources' links;
    /// `None` when the link is refused, and the read is never made.
    link: Option<LinkConfig>,
    /// The ComParams of the link other than the timings, which the sources of one read share.
    non_timing: Vec<(String, u32)>,
    /// The document of the sources whose link is refused.
    refused: Option<Sha256>,
    /// The whole request, SID first.
    request: Vec<u8>,
    /// The sources decoded from the one response, with how each is read.
    declarations: Vec<(Source, ServiceField)>,
}

/// A document's identification requests: each pattern service with its whole request.
type DocumentRequests = (Sha256, Vec<(u32, Vec<u8>)>);

/// A (document, service) declaration of a request: `pattern` when a variant pattern names it,
/// which counts for conclusiveness (ADR-327 item 3); a rule-only declaration is read for a field
/// rule's source (ADR-403).
#[derive(Debug, Clone, Copy)]
struct Declaration {
    document: Sha256,
    service: u32,
    pattern: bool,
}

/// One identification request of a link group and the declarations that share its bytes.
#[derive(Debug)]
struct RequestPlan {
    /// The whole request, SID first.
    request: Vec<u8>,
    /// The declarations whose identification request these bytes are.
    declarations: Vec<Declaration>,
}

/// The variants of one definition at one endpoint that share a link: equal ComParams other than
/// the timings (ADR-401).
#[derive(Debug)]
struct LinkGroup {
    /// The first document's declared link with the largest timings of the group; `None` when the
    /// documents' link is refused and nothing opens.
    link: Option<LinkConfig>,
    /// The ComParams of the link other than the timings.
    non_timing: Vec<(String, u32)>,
    documents: Vec<Sha256>,
    requests: Vec<RequestPlan>,
}

impl LinkGroup {
    /// Whether the run opens a link for the group: it has a link and a request to send.
    fn opens(&self) -> bool {
        self.link.is_some() && !self.requests.is_empty()
    }
}

/// The read of one ECU definition at one endpoint: its variants grouped by declared link
/// (ADR-401).
#[derive(Debug)]
struct EcuReadPlan {
    definition: String,
    /// `None` for the documents whose link ADR-324 refuses.
    endpoint: Option<EndpointKey>,
    groups: Vec<LinkGroup>,
}

/// What a field rule's source decodes to, as far as its declaration tells (ADR-403): text or not,
/// and the data kind with the bit length of a fixed-length field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceType {
    text: bool,
    fixed: Option<(DataKind, u32)>,
}

impl SourceType {
    /// Whether a response could decode to one value through both: a text and bytes never equal,
    /// and fields of one data kind at different fixed lengths never agree.
    fn can_agree(self, other: Self) -> bool {
        self.text == other.text
            && match (self.fixed, other.fixed) {
                (Some((a, bits)), Some((b, other_bits))) if a == b => bits == other_bits,
                _ => true,
            }
    }
}

/// The field rules' sources of one component on one request of one link group of one read: the
/// sources decode the one response, and the component's value is established when they agree
/// (ADR-327 item 4, ADR-403).
#[derive(Debug)]
struct RuleFieldPlan {
    /// Index into `CheckedPlan::ecu_reads` (a definition at an endpoint).
    read: usize,
    /// Indexes of the link group and of its request.
    group: usize,
    request: usize,
    component: Component,
    /// How the sources' values read a rule's expected value; equal for every source.
    kind: ExpectedKind,
    /// What each source decodes to, which the plan check requires to be able to agree.
    types: Vec<SourceType>,
    /// Deduplicated, in plan order (candidate order, then rule order).
    sources: Vec<Source>,
}

/// A plan that passed [`check_plan`].
#[derive(Debug)]
pub struct CheckedPlan {
    plan_sha256: Sha256,
    selection: Selection,
    candidates: Vec<String>,
    links: Vec<DocumentLink>,
    refused_links: Vec<RefusedLink>,
    /// The VIN reads, in the order the plan's VIN rules first name their sources.
    vin_reads: Vec<VinReadPlan>,
    /// Every candidate's VIN rules, with the candidate's id, in plan order.
    vin_rules: Vec<(String, VinRule)>,
    /// The documents the plan names, which the ECU reads decode with.
    documents: Vec<(Sha256, JobDocument)>,
    /// The ECU reads, definition by definition in plan order (ADR-401).
    ecu_reads: Vec<EcuReadPlan>,
    /// Every candidate's entries with the candidate's id, in plan order.
    entries: Vec<(String, PlanEntry)>,
    /// Every candidate's field rules, with the candidate's id, in plan order.
    field_rules: Vec<(String, FieldRule)>,
    /// The rule fields of the ECU reads, in plan order (ADR-403).
    rule_fields: Vec<RuleFieldPlan>,
}

impl EcuReadPlan {
    /// Whether the run opens a link for some group of the read.
    fn opens(&self) -> bool {
        self.groups.iter().any(LinkGroup::opens)
    }
}

impl CheckedPlan {
    /// Whether some ECU read opens a link.
    fn ecu_reads_to_do(&self) -> bool {
        self.ecu_reads.iter().any(EcuReadPlan::opens)
    }
}

/// The checks that run before any link opens: every service a pattern or a rule source names
/// is read-only and declared, every VIN rule's source is a 17-byte ASCII field in the shape of
/// the ADR-345 table (ADR-399 item 2), every document's link is derived (a link ADR-324 refuses
/// is reported, not refused), and the links of one bus agree in their ComParams other than the
/// timings.
pub fn check_plan(loaded: &LoadedPlan) -> Result<CheckedPlan, PlanRefused> {
    let document = |hash: &Sha256| {
        let found = loaded.documents.iter().find(|(h, _)| h == hash);
        &found
            .expect("validate: every source names a listed document")
            .1
    };
    // Every document's identification requests, built before any link opens (ADR-401).
    let mut requests: Vec<DocumentRequests> = Vec::new();
    for (hash, document) in &loaded.documents {
        let variant = document.variant();
        for pattern in variant.patterns().into_iter().flatten() {
            check_read_only(hash, &variant, pattern.service_id())?;
        }
        let built = identification_requests(&variant).map_err(|error| {
            PlanRefused::IdentificationRequest {
                document: hex(hash),
                service: match &error {
                    IdentifyError::Incomplete(service, _)
                    | IdentifyError::Encode { service, .. } => *service,
                    _ => 0,
                },
                why: error.to_string(),
            }
        })?;
        requests.push((*hash, built));
    }
    // The VIN rules' sources, each once, in plan order.
    let mut vin_sources: Vec<(Source, ServiceField)> = Vec::new();
    // Every field rule's whole request and expected kind, in plan order (ADR-403).
    let mut field_checks: Vec<(Vec<u8>, ExpectedKind, SourceType)> = Vec::new();
    for candidate in &loaded.plan.candidates {
        let sources = candidate
            .vin_rules
            .iter()
            .map(|rule| &rule.source)
            .chain(candidate.field_rules.iter().map(|rule| &rule.source));
        for source in sources {
            let variant = document(&source.document).variant();
            check_read_only(&source.document, &variant, source.service)?;
            let has_field = variant
                .services()
                .into_iter()
                .flatten()
                .find(|service| service.id() == source.service)
                .and_then(|service| service.positive()?.fields())
                .is_some_and(|fields| fields.iter().any(|field| field.id() == source.field));
            if !has_field {
                return Err(PlanRefused::UnknownField {
                    document: hex(&source.document),
                    service: source.service,
                    field: source.field,
                });
            }
        }
        for rule in &candidate.field_rules {
            let variant = document(&rule.source.document).variant();
            field_checks.push(check_field_rule(&variant, rule)?);
        }
        for rule in &candidate.vin_rules {
            if vin_sources.iter().any(|(known, _)| *known == rule.source) {
                continue;
            }
            let variant = document(&rule.source.document).variant();
            let vin_source = |why: String| PlanRefused::VinSource {
                document: hex(&rule.source.document),
                service: rule.source.service,
                field: rule.source.field,
                why,
            };
            let field = vin_source_field(&variant, rule.source.service, rule.source.field)
                .map_err(vin_source)?;
            // The shape the table accepts of one entry (a single-identifier read); the ids are
            // local to a document, so entries of several documents are not tabled together.
            ServiceSources::new(vec![field.clone()])
                .map_err(|error| vin_source(error.to_string()))?;
            vin_sources.push((rule.source, field));
        }
    }
    let mut links: Vec<DocumentLink> = Vec::new();
    let mut refused_links = Vec::new();
    for (hash, document) in &loaded.documents {
        let variant = document.variant();
        match LinkConfig::declared(&variant) {
            Ok(link) => {
                // `declared` accepted exactly one protocol.
                let functional_id = variant
                    .protocols()
                    .and_then(|protocols| protocols.iter().next())
                    .map_or(0, |protocol| protocol.functional_id());
                if functional_id > MAX_CAN_ID {
                    return Err(PlanRefused::BadFunctionalId {
                        document: hex(hash),
                        functional_id,
                        why: "only 11-bit identifiers are supported, as for the physical ones",
                    });
                }
                let new_bus = BusKey::of(&link);
                for other in &links {
                    let bus = BusKey::of(&other.link);
                    let same_resource =
                        bus.protocol == new_bus.protocol && bus.bus_type == new_bus.bus_type;
                    if same_resource && bus.baud_rate != new_bus.baud_rate {
                        return Err(PlanRefused::BaudRatesDiffer {
                            first: hex(&other.document),
                            second: hex(hash),
                        });
                    }
                    // Only links that send one functional request together must agree.
                    let grouped = bus == new_bus
                        && functional_id != 0
                        && other.functional_id == functional_id;
                    if grouped && comparams(&other.link) != comparams(&link) {
                        return Err(PlanRefused::ComParamsDiffer {
                            first: hex(&other.document),
                            second: hex(hash),
                        });
                    }
                }
                links.push(DocumentLink {
                    document: *hash,
                    link,
                    functional_id,
                });
            }
            Err(error) => refused_links.push(RefusedLink {
                document: *hash,
                reason: error.to_string(),
            }),
        }
    }
    for link in links.iter().filter(|link| link.functional_id != 0) {
        let clash = links.iter().any(|other| {
            BusKey::of(&other.link) == BusKey::of(&link.link)
                && [other.link.tx_id, other.link.rx_id].contains(&link.functional_id)
        });
        if clash {
            return Err(PlanRefused::BadFunctionalId {
                document: hex(&link.document),
                functional_id: link.functional_id,
                why: "it equals a physical request or response identifier on the same bus",
            });
        }
    }
    let candidates = loaded
        .plan
        .candidates
        .iter()
        .map(|c| c.id.clone())
        .collect();
    let vin_reads = group_vin_reads(vin_sources, &links);
    let vin_rules = loaded
        .plan
        .candidates
        .iter()
        .flat_map(|c| c.vin_rules.iter().map(|rule| (c.id.clone(), rule.clone())))
        .collect();
    let mut ecu_reads = group_ecu_reads(&loaded.plan, &requests, &links, &refused_links);
    let field_rules: Vec<(String, FieldRule)> = loaded
        .plan
        .candidates
        .iter()
        .flat_map(|c| {
            c.field_rules
                .iter()
                .map(|rule| (c.id.clone(), rule.clone()))
        })
        .collect();
    let rule_fields = group_rule_fields(&field_rules, &field_checks, &mut ecu_reads)?;
    let entries = loaded
        .plan
        .candidates
        .iter()
        .flat_map(|c| c.entries.iter().map(|entry| (c.id.clone(), entry.clone())))
        .collect();
    Ok(CheckedPlan {
        plan_sha256: loaded.plan_sha256,
        selection: loaded.plan.selection,
        candidates,
        links,
        refused_links,
        vin_reads,
        vin_rules,
        documents: loaded.documents.clone(),
        ecu_reads,
        entries,
        field_rules,
        rule_fields,
    })
}

/// What the plan check requires of one field rule's source: its request builds without values,
/// its field is one a variant pattern could compare (ADR-306 item 5), and its expected value could
/// match some value of the field (`ExpectedKind::never_matches`: not empty, within what the
/// field's width holds, no longer than a fixed-length field, UTF-8 for text). The source's whole
/// request, the kind its field's value reads `expected` by and what it decodes to.
fn check_field_rule(
    variant: &EcuVariant<'_>,
    rule: &FieldRule,
) -> Result<(Vec<u8>, ExpectedKind, SourceType), PlanRefused> {
    let source = &rule.source;
    let refused = |why: String| PlanRefused::FieldSource {
        document: hex(&source.document),
        service: source.service,
        field: source.field,
        why,
    };
    let request = identification_request_of(variant, source.service)
        .map_err(|error| refused(error.to_string()))?;
    let field = variant
        .services()
        .into_iter()
        .flatten()
        .find(|service| service.id() == source.service)
        .and_then(|service| service.positive()?.fields())
        .and_then(|fields| fields.iter().find(|field| field.id() == source.field))
        .ok_or_else(|| PlanRefused::UnknownField {
            document: hex(&source.document),
            service: source.service,
            field: source.field,
        })?;
    let kind = ExpectedKind::of_field(&field).map_err(|reason| {
        refused(format!(
            "a field a variant pattern could not compare: {reason}"
        ))
    })?;
    if let Some(why) = kind.never_matches(&field, &rule.expected) {
        return Err(refused(why.to_owned()));
    }
    let fixed = (field.length_kind() == LengthKind::Fixed && field.bit_length() >= 1)
        .then(|| (field.kind(), field.bit_length()));
    let text = matches!(
        field.kind(),
        DataKind::Ascii | DataKind::Utf8 | DataKind::Unicode2
    );
    Ok((request, kind, SourceType { text, fixed }))
}

/// Joins each field rule's source to the read of its document's definition (ADR-403): the source
/// is a declaration of the request its service builds, in the link group that holds its document,
/// flagged as no pattern's when none names it; one rule field per (read, group, request,
/// component) holds the sources of its rules. `checks` gives each rule's request and kind, in the
/// order of `rules`. Refused when the sources of one rule field read their value by different
/// kinds.
fn group_rule_fields(
    rules: &[(String, FieldRule)],
    checks: &[(Vec<u8>, ExpectedKind, SourceType)],
    reads: &mut [EcuReadPlan],
) -> Result<Vec<RuleFieldPlan>, PlanRefused> {
    let mut fields: Vec<RuleFieldPlan> = Vec::new();
    for ((_, rule), (request, kind, source_type)) in rules.iter().zip(checks) {
        let source = rule.source;
        // The document is in exactly one group of one read (`validate`, `group_ecu_reads`).
        let Some((read, group)) = reads.iter().enumerate().find_map(|(read, planned)| {
            let group = planned
                .groups
                .iter()
                .position(|group| group.documents.contains(&source.document))?;
            Some((read, group))
        }) else {
            continue;
        };
        let requests = &mut reads[read].groups[group].requests;
        let at = requests
            .iter()
            .position(|known| known.request == *request)
            .unwrap_or_else(|| {
                requests.push(RequestPlan {
                    request: request.clone(),
                    declarations: Vec::new(),
                });
                requests.len() - 1
            });
        let declarations = &mut requests[at].declarations;
        if !declarations
            .iter()
            .any(|d| d.document == source.document && d.service == source.service)
        {
            declarations.push(Declaration {
                document: source.document,
                service: source.service,
                pattern: false,
            });
        }
        let known = fields.iter().position(|f| {
            f.read == read && f.group == group && f.request == at && f.component == rule.component
        });
        let field = match known {
            Some(known) => &mut fields[known],
            None => {
                fields.push(RuleFieldPlan {
                    read,
                    group,
                    request: at,
                    component: rule.component,
                    kind: *kind,
                    types: Vec::new(),
                    sources: Vec::new(),
                });
                fields.last_mut().expect("just pushed")
            }
        };
        if field.kind != *kind {
            return Err(PlanRefused::FieldSource {
                document: hex(&source.document),
                service: source.service,
                field: source.field,
                why: "sources of one component on one request decode to different kinds".to_owned(),
            });
        }
        if !field
            .types
            .iter()
            .all(|known| known.can_agree(*source_type))
        {
            return Err(PlanRefused::FieldSource {
                document: hex(&source.document),
                service: source.service,
                field: source.field,
                why: "sources of one component on one request cannot decode to one value (a text and bytes, or one data kind at two fixed lengths)"
                    .to_owned(),
            });
        }
        if !field.sources.contains(&source) {
            field.sources.push(source);
            field.types.push(*source_type);
        }
    }
    Ok(fields)
}

/// The endpoint a declared link goes to.
fn endpoint_of(link: &LinkConfig) -> EndpointKey {
    EndpointKey {
        bus: BusKey::of(link),
        tx_id: link.tx_id,
        rx_id: link.rx_id,
    }
}

/// Groups the plan's definitions into ECU reads: per definition, in plan order, its documents in
/// their listed order by endpoint, and within an endpoint by ComParams other than the timings;
/// each group's link has the largest timings of its documents' links, and a request of equal
/// bytes is one request of the group for every declaration that builds it (ADR-401). The
/// documents whose link is refused form a read without an endpoint, a group each.
fn group_ecu_reads(
    plan: &IdentificationPlan,
    requests: &[DocumentRequests],
    links: &[DocumentLink],
    refused_links: &[RefusedLink],
) -> Vec<EcuReadPlan> {
    let mut reads: Vec<EcuReadPlan> = Vec::new();
    for definition in &plan.definitions {
        for (at, hash) in definition.documents.iter().enumerate() {
            // A document listed twice under one definition is read once.
            if definition.documents[..at].contains(hash) {
                continue;
            }
            let built = requests
                .iter()
                .find(|(known, _)| known == hash)
                .map_or(&[][..], |(_, built)| built.as_slice());
            let declared = links.iter().find(|l| l.document == *hash);
            let refused = refused_links.iter().any(|r| r.document == *hash);
            let endpoint = match (declared, refused) {
                (Some(declared), _) => Some(endpoint_of(&declared.link)),
                (None, true) => None,
                // `check_plan` puts every document in one of the two.
                (None, false) => continue,
            };
            let at = reads
                .iter()
                .position(|r| r.definition == definition.id && r.endpoint == endpoint)
                .unwrap_or_else(|| {
                    reads.push(EcuReadPlan {
                        definition: definition.id.clone(),
                        endpoint: endpoint.clone(),
                        groups: Vec::new(),
                    });
                    reads.len() - 1
                });
            let groups = &mut reads[at].groups;
            let non_timing = declared.map(|d| comparams(&d.link)).unwrap_or_default();
            let known =
                declared.and_then(|_| groups.iter().position(|g| g.non_timing == non_timing));
            let group = match (known, declared) {
                (Some(known), Some(declared)) => {
                    let group = &mut groups[known];
                    if let Some(link) = &mut group.link {
                        raise_timings(link, &declared.link);
                    }
                    group
                }
                _ => {
                    groups.push(LinkGroup {
                        link: declared.map(|d| d.link.clone()),
                        non_timing,
                        documents: Vec::new(),
                        requests: Vec::new(),
                    });
                    groups.last_mut().expect("just pushed")
                }
            };
            if !group.documents.contains(hash) {
                group.documents.push(*hash);
            }
            for (service, request) in built {
                match group.requests.iter_mut().find(|r| r.request == *request) {
                    Some(known) => known.declarations.push(Declaration {
                        document: *hash,
                        service: *service,
                        pattern: true,
                    }),
                    None => group.requests.push(RequestPlan {
                        request: request.clone(),
                        declarations: vec![Declaration {
                            document: *hash,
                            service: *service,
                            pattern: true,
                        }],
                    }),
                }
            }
        }
    }
    reads
}

/// Groups the VIN sources into reads: sources on one endpoint with equal ComParams other than the
/// timings and the same request share one read, whose link has the largest timings of theirs
/// (ADR-399 item 1). The reads are in the order their first source comes in the plan. Sources of
/// a document whose link is refused are grouped by document and request and never read.
fn group_vin_reads(
    sources: Vec<(Source, ServiceField)>,
    links: &[DocumentLink],
) -> Vec<VinReadPlan> {
    let mut reads: Vec<VinReadPlan> = Vec::new();
    for (source, field) in sources {
        let declared = links.iter().find(|l| l.document == source.document);
        let endpoint = declared.map(|d| EndpointKey {
            bus: BusKey::of(&d.link),
            tx_id: d.link.tx_id,
            rx_id: d.link.rx_id,
        });
        let non_timing = declared.map(|d| comparams(&d.link)).unwrap_or_default();
        let refused = declared.is_none().then_some(source.document);
        let known = reads.iter_mut().find(|read| {
            read.endpoint == endpoint
                && read.non_timing == non_timing
                && read.refused == refused
                && read.request == field.request
        });
        match (known, declared) {
            (Some(read), Some(declared)) => {
                if let Some(link) = &mut read.link {
                    raise_timings(link, &declared.link);
                }
                read.declarations.push((source, field));
            }
            (Some(read), None) => read.declarations.push((source, field)),
            (None, _) => reads.push(VinReadPlan {
                endpoint,
                link: declared.map(|d| d.link.clone()),
                non_timing,
                refused,
                request: field.request.clone(),
                declarations: vec![(source, field)],
            }),
        }
    }
    reads
}

/// Whether the request of declared service `service` is in its layer's read-only set
/// (ADR-327 item 1). A variant whose protocols name no layer the agent has a list for judges
/// nothing read-only, as [`ApplicationLayer::of_job`] does.
fn check_read_only(
    hash: &Sha256,
    variant: &EcuVariant<'_>,
    service: u32,
) -> Result<(), PlanRefused> {
    let allowed = ApplicationLayer::of_job(Some(variant))
        .zip(declared_sid(variant, service))
        .is_some_and(|(layer, sid)| layer.read_only_services().contains(&sid));
    allowed
        .then_some(())
        .ok_or_else(|| PlanRefused::NotReadOnly {
            document: hex(hash),
            service,
        })
}

/// The timing ComParams that `LinkConfig` keeps in its params (the three it models as fields,
/// `CP_P2Max`, `CP_P2Star` and `CP_RC78CompletionTimeout`, are timings too). This is the
/// agent's own list of the timings the repository deals with (ADR-395 item 6); a name that is
/// not on it counts as a non-timing ComParam, which the links of a group must agree on.
const TIMING_PARAMS: [&str; 8] = [
    "CP_P2Min",
    "CP_P3Func",
    "CP_As",
    "CP_Ar",
    "CP_Bs",
    "CP_Br",
    "CP_Cr",
    "CP_Cs",
];

fn is_timing(name: &str) -> bool {
    TIMING_PARAMS.iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// A link's ComParams other than the timings, by lowercase name, in a fixed order.
fn comparams(link: &LinkConfig) -> Vec<(String, u32)> {
    let mut params: Vec<_> = link
        .params
        .iter()
        .filter(|(name, _)| !is_timing(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), *value))
        .collect();
    params.sort();
    params
}

/// What one VIN source gave (ADR-399 item 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum VinOutcome {
    /// A well-formed VIN, decoded from the response. `Debug` hides it (design 16.2).
    Decoded(Vin),
    /// The field decoded to text that is not a well-formed VIN.
    NotWellFormed,
    /// The response carried no field that decodes.
    Undecodable,
    /// A negative response, with its response code.
    Negative(u8),
    /// The request got no usable answer within the run's repeats: none at all, the link's
    /// error event for it, a receive error or lost events (each a failure of the ECU or the
    /// bus, ADR-392).
    NoUsableAnswer,
    /// The source's document declares a link ADR-324 refuses, so nothing was read: known
    /// before any link opens.
    LinkRefused,
    /// The worker refused to open the source's link (a connect it refused, or another failure
    /// to open that left nothing open) when the read was made.
    WorkerRefused,
    /// The read was not made: the run ended, or its bounds ran out, before it.
    NotRead,
}

/// A VIN rule source of one read, with what it gave.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VinSourceRead {
    #[serde(serialize_with = "as_hex")]
    pub document: Sha256,
    pub service: u32,
    pub field: u32,
    pub outcome: VinOutcome,
}

/// One read of the VIN: one request on one endpoint, and every source its response is decoded by
/// (ADR-399 item 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VinRead {
    /// The endpoint read; `None` when the sources' link is refused.
    pub endpoint: Option<EndpointKey>,
    #[serde(serialize_with = "as_hex")]
    pub request: Vec<u8>,
    pub sources: Vec<VinSourceRead>,
    /// The VIN when every source gave the same well-formed one.
    pub accepted: Option<Vin>,
}

/// Whether the established VIN satisfies a VIN rule, or the established value of a rule field a
/// field rule. Only a plan whose selection narrows drops a candidate on it, per component (and
/// rule field, for a field rule) (ADR-402, ADR-399 item 6, ADR-403).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RuleOutcome {
    Matched,
    NotMatched,
    /// No VIN was established, or the rule field's value was not.
    Unevaluated,
}

/// A candidate's VIN rule and how the established VIN stands to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VinRuleResult {
    pub candidate: String,
    pub component: Component,
    pub value: String,
    pub start: u8,
    pub end: u8,
    pub outcome: RuleOutcome,
}

/// A decoded value of an identification field. It is built from the decoder's physical value, so
/// bytes show as the lowercase hex the report shows them in (ADR-396 item 7, ADR-401) and the IR
/// crate commits to no JSON shape. `Debug` shows the kind and the length only: a field can hold
/// the VIN, which design 16.2 keeps out of debug output.
#[derive(Clone, PartialEq, Serialize)]
pub enum FieldValue {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
    Text(String),
    Bytes(#[serde(serialize_with = "as_hex")] Vec<u8>),
    Group(Vec<IdentificationField>),
    Repeat(Vec<Vec<IdentificationField>>),
    Select {
        case: Option<usize>,
        fields: Vec<IdentificationField>,
    },
}

impl std::fmt::Debug for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsigned(_) => f.write_str("Unsigned(<redacted>)"),
            Self::Signed(_) => f.write_str("Signed(<redacted>)"),
            Self::Float(_) => f.write_str("Float(<redacted>)"),
            Self::Text(text) => write!(f, "Text(<{} bytes>)", text.len()),
            Self::Bytes(bytes) => write!(f, "Bytes(<{} bytes>)", bytes.len()),
            Self::Group(fields) => f.debug_tuple("Group").field(fields).finish(),
            Self::Repeat(items) => f.debug_tuple("Repeat").field(items).finish(),
            Self::Select { case, fields } => f
                .debug_struct("Select")
                .field("case", case)
                .field("fields", fields)
                .finish(),
        }
    }
}

/// A field of a response to an identification request, decoded through a variant's positive
/// response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdentificationField {
    pub id: u32,
    pub short_name: String,
    pub value: FieldValue,
}

impl From<&DecodedField> for IdentificationField {
    fn from(field: &DecodedField) -> Self {
        Self {
            id: field.id,
            short_name: field.short_name.clone(),
            value: FieldValue::from(&field.value),
        }
    }
}

fn fields_of(fields: &[DecodedField]) -> Vec<IdentificationField> {
    fields.iter().map(IdentificationField::from).collect()
}

impl From<&PhysicalValue> for FieldValue {
    fn from(value: &PhysicalValue) -> Self {
        match value {
            PhysicalValue::Unsigned(n) => Self::Unsigned(*n),
            PhysicalValue::Signed(n) => Self::Signed(*n),
            PhysicalValue::Float(n) => Self::Float(*n),
            PhysicalValue::Text(text) => Self::Text(text.clone()),
            PhysicalValue::Bytes(bytes) => Self::Bytes(bytes.clone()),
            PhysicalValue::Group(fields) => Self::Group(fields_of(fields)),
            PhysicalValue::Repeat(items) => {
                Self::Repeat(items.iter().map(|i| fields_of(i)).collect())
            }
            PhysicalValue::Select { case, fields } => Self::Select {
                case: *case,
                fields: fields_of(fields),
            },
        }
    }
}

/// What one declaration gave: the response to the identification request it shares, decoded
/// through its service (ADR-401).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum RequestOutcome {
    /// A positive response that decodes through the declaration's service.
    Decoded(Vec<IdentificationField>),
    /// A response that is not negative and does not decode through the service.
    Undecodable,
    /// A negative response, with its response code.
    Negative(u8),
    /// The request got no usable answer within the run's repeats (ADR-392).
    NoUsableAnswer,
    /// The declaration's document declares a link ADR-324 refuses, so nothing was read.
    LinkRefused,
    /// The worker refused to open the link when the read was made.
    WorkerRefused,
    /// The read was not made: the run ended, or its bounds ran out, before it.
    NotRead,
    /// The read was not made: no remaining candidate named the definition when the reads ended
    /// (ADR-402).
    NotInPlay,
}

/// A (document, service) declaration of an identification request and what it gave.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeclarationOutcome {
    #[serde(serialize_with = "as_hex")]
    pub document: Sha256,
    pub service: u32,
    /// A variant pattern names the service; else it is read only for a field rule's source and
    /// counts for nothing in conclusiveness (ADR-403).
    pub pattern: bool,
    pub outcome: RequestOutcome,
}

/// What one field rule source gave (ADR-403): the field's value, or why there is none.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum RuleSourceOutcome {
    /// The response decoded and held the field.
    Decoded(FieldValue),
    /// A response that is not negative and does not decode through the source's service.
    Undecodable,
    /// A negative response, with its response code.
    Negative(u8),
    /// The request got no usable answer within the run's repeats (ADR-392).
    NoUsableAnswer,
    /// The source's document declares a link ADR-324 refuses, so nothing was read.
    LinkRefused,
    /// The worker refused to open the link when the read was made.
    WorkerRefused,
    /// The read was not made: the run ended, or its bounds ran out, before it.
    NotRead,
    /// The read was not made: no remaining candidate named the definition (ADR-402).
    NotInPlay,
}

/// A field rule source of a rule field, with what it gave.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleFieldSource {
    #[serde(serialize_with = "as_hex")]
    pub document: Sha256,
    pub service: u32,
    pub field: u32,
    pub outcome: RuleSourceOutcome,
}

/// A candidate's field rule and how the rule field's established value stands to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldRuleResult {
    pub candidate: String,
    pub value: String,
    #[serde(serialize_with = "as_hex")]
    pub document: Sha256,
    pub service: u32,
    pub field: u32,
    #[serde(serialize_with = "as_hex")]
    pub expected: Vec<u8>,
    pub outcome: RuleOutcome,
}

/// One component's field rules on one request of one definition's read at one endpoint: the
/// sources that decode its response, the value established when they agree, and every candidate's
/// rules on it (ADR-327 items 4 and 6, ADR-403).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleFieldRead {
    pub definition: String,
    /// The endpoint read; `None` when the sources' link is refused.
    pub endpoint: Option<EndpointKey>,
    /// The index of the link group within the definition's read at the endpoint, in the order
    /// the groups are planned: two groups can read one request.
    pub group: usize,
    #[serde(serialize_with = "as_hex")]
    pub request: Vec<u8>,
    pub component: Component,
    pub sources: Vec<RuleFieldSource>,
    /// The value every source decoded to, when they all decoded and agree.
    pub established: Option<FieldValue>,
    pub rules: Vec<FieldRuleResult>,
}

/// A value of a component that a matched rule gives (ADR-403).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentValue {
    pub component: Component,
    pub value: String,
}

/// One identification request, sent once, and every declaration its one response is decoded by.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdentificationRequest {
    #[serde(serialize_with = "as_hex")]
    pub request: Vec<u8>,
    pub declarations: Vec<DeclarationOutcome>,
}

/// The read of one ECU definition at one endpoint (ADR-401).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EcuRead {
    pub definition: String,
    /// The endpoint read; `None` for the documents whose link ADR-324 refuses.
    pub endpoint: Option<EndpointKey>,
    #[serde(serialize_with = "hashes_as_hex")]
    pub documents: Vec<Sha256>,
    /// Every distinct request of every link group, in group order.
    pub requests: Vec<IdentificationRequest>,
    /// Every document whose variant matched (ADR-327 item 7).
    #[serde(serialize_with = "hashes_as_hex")]
    pub matching: Vec<Sha256>,
    /// Every pattern declaration decoded a positive response and no variant of the definition has
    /// a link ADR-324 refuses: a read that rules out the definition's other variants (ADR-327
    /// item 3). No negative response is definitive yet, so any one leaves the read inconclusive;
    /// a request read only for a field rule's source counts for nothing here (ADR-403).
    pub conclusive: bool,
    /// Some declaration got a response of the ECU, a rule-only one included.
    pub answered: bool,
}

/// How an entry of a candidate stands after the reads (ADR-326 item 3, ADR-327, ADR-401).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum EntryState {
    /// Exactly one alternative has a matching variant.
    Identified {
        definition: String,
        #[serde(serialize_with = "hashes_as_hex")]
        variants: Vec<Sha256>,
    },
    /// Two or more alternatives have a matching variant.
    Ambiguous { definitions: Vec<String> },
    /// An endpoint answered, and every alternative read there was read conclusively without a
    /// match.
    UnrecognizedVariant { endpoints: Vec<EndpointKey> },
    /// An endpoint answered, but an alternative read there, or a variant of it, stayed
    /// inconclusive.
    AnsweredNotIdentified,
    /// A request was sent and nothing answered, for a mandatory entry.
    NotAnswering,
    /// A request was sent and nothing answered, for an optional entry.
    NotFitted,
    /// No request was sent for the entry: the reads never ran, were refused or were interrupted.
    NotRead,
}

/// A candidate's entry and its state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntryResult {
    pub candidate: String,
    pub role: String,
    pub optional: bool,
    pub alternatives: Vec<String>,
    pub state: EntryState,
}

/// Why a candidate was dropped (ADR-327 item 3, ADR-402).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Contradiction {
    /// A definition the candidate lists in no entry was identified at an endpoint one of its
    /// entries uses, and every definition the candidate lists there was read there conclusively
    /// without a match.
    UnlistedDefinition {
        definition: String,
        endpoint: EndpointKey,
        ruled_out: Vec<String>,
    },
    /// The VIN is established and none of the candidate's VIN rules for the component matched.
    VinRules { component: Component },
    /// A rule field of the component is established and none of the candidate's rules on it
    /// matched (ADR-403).
    FieldRules {
        component: Component,
        definition: String,
        endpoint: EndpointKey,
        /// The link group of the rule field within the read (see `RuleFieldRead::group`).
        group: usize,
        #[serde(serialize_with = "as_hex")]
        request: Vec<u8>,
    },
}

/// A candidate a plan whose selection narrows dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedCandidate {
    pub candidate: String,
    /// The definition whose read completed the contradiction; `None` when the VIN did.
    pub after: Option<String>,
    pub contradictions: Vec<Contradiction>,
}

/// One step of the ECU phase: the definition read, with the worst case that chose it
/// (ADR-327 item 4, ADR-402).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadStep {
    pub definition: String,
    /// The most candidates any conclusive outcome of reading the definition leaves.
    pub worst_case: usize,
}

/// What the plan's checks and the derivation give (design 8.8.2), and, once
/// [`run_identification`] has run, the presence section (which response identifiers answered
/// the functional presence requests) and the VIN section (ADR-399).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IdentificationReport {
    selection: Selection,
    candidates: Vec<String>,
    /// The candidates no contradiction dropped: all of them for a plan whose selection does not
    /// narrow (ADR-402).
    remaining: Vec<String>,
    endpoints: Vec<EndpointEntry>,
    presence_groups: Vec<PresenceGroup>,
    buses_without_presence: Vec<BusKey>,
    refused_links: Vec<RefusedLink>,
    presence: Vec<PresenceAnswer>,
    /// How many presence links the run opened (one per part of each group's halved table).
    presence_links_opened: u32,
    refused_tables: Vec<RefusedTable>,
    /// A presence request could not be completed, because the path to the VCI was lost and the
    /// run's bounds ran out: the presence section holds what was heard before.
    presence_interrupted: bool,
    /// The vehicle's VIN: every accepted VIN read gave this one, and the VIN phase was not
    /// interrupted (ADR-399 item 3).
    vin: Option<Vin>,
    /// Each distinct VIN a read accepted, in the order they came.
    vins_seen: Vec<Vin>,
    /// The VIN reads in plan order, each source `NotRead` or `LinkRefused` until the run reads it.
    vin_reads: Vec<VinRead>,
    /// The VIN phase did not finish, because the presence phase was interrupted or the run's
    /// bounds ran out while a VIN read was made.
    vin_interrupted: bool,
    /// The identified VIN's per-vehicle lock is held by the job's guards.
    vehicle_lock_held: bool,
    /// Each candidate's VIN rules against the established VIN.
    vin_rules: Vec<VinRuleResult>,
    /// The rule fields of the ECU reads in plan order, each source `NotRead` or `LinkRefused`
    /// and each rule unevaluated until the run reads them (ADR-403).
    rule_fields: Vec<RuleFieldRead>,
    /// The distinct component values the matched VIN and field rules give, VIN rules first.
    component_values: Vec<ComponentValue>,
    /// The candidates dropped, in the order they were (ADR-402).
    dropped: Vec<DroppedCandidate>,
    /// The definitions read, in the order read, each with the worst case that chose it.
    read_order: Vec<ReadStep>,
    /// The ECU reads in plan order, each declaration `NotRead` or `LinkRefused` until the run
    /// reads it (ADR-401).
    ecu_reads: Vec<EcuRead>,
    /// The ECU phase did not finish, because an earlier phase was interrupted or the run's
    /// bounds ran out while an ECU read was made.
    ecu_reads_interrupted: bool,
    /// Every candidate's entries in plan order, `NotRead` until the ECU phase has run.
    entries: Vec<EntryResult>,
}

impl IdentificationReport {
    /// How the vehicle was chosen; only `Selection::Candidates` narrows (ADR-327 item 8).
    pub fn selection(&self) -> Selection {
        self.selection
    }
    /// The ids of the plan's candidates.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }
    /// The candidates left after the reads and the VIN rules; all of them for a plan whose
    /// selection does not narrow (ADR-402).
    pub fn remaining(&self) -> &[String] {
        &self.remaining
    }
    /// The candidates dropped, in order, each with its contradictions.
    pub fn dropped(&self) -> &[DroppedCandidate] {
        &self.dropped
    }
    /// The definitions in the order the ECU phase read them (ecu_reads stays in plan order).
    pub fn read_order(&self) -> &[ReadStep] {
        &self.read_order
    }
    /// Each endpoint once, with the documents whose link is on it, in plan order.
    pub fn endpoints(&self) -> &[EndpointEntry] {
        &self.endpoints
    }
    pub fn presence_groups(&self) -> &[PresenceGroup] {
        &self.presence_groups
    }
    /// Buses on which no declared link has a functional identifier, so no presence request
    /// can be sent there.
    pub fn buses_without_presence(&self) -> &[BusKey] {
        &self.buses_without_presence
    }
    pub fn refused_links(&self) -> &[RefusedLink] {
        &self.refused_links
    }
    pub fn presence(&self) -> &[PresenceAnswer] {
        &self.presence
    }
    pub fn presence_links_opened(&self) -> u32 {
        self.presence_links_opened
    }
    pub fn refused_tables(&self) -> &[RefusedTable] {
        &self.refused_tables
    }
    pub fn presence_interrupted(&self) -> bool {
        self.presence_interrupted
    }
    /// The VIN the job established: `None` when no VIN rule names a source, no read gave a
    /// well-formed VIN, reads disagreed, or the VIN phase was interrupted (ADR-399).
    pub fn vin(&self) -> Option<&Vin> {
        self.vin.as_ref()
    }
    pub fn vins_seen(&self) -> &[Vin] {
        &self.vins_seen
    }
    pub fn vin_reads(&self) -> &[VinRead] {
        &self.vin_reads
    }
    pub fn vin_interrupted(&self) -> bool {
        self.vin_interrupted
    }
    pub fn vehicle_lock_held(&self) -> bool {
        self.vehicle_lock_held
    }
    pub fn vin_rules(&self) -> &[VinRuleResult] {
        &self.vin_rules
    }
    pub fn ecu_reads(&self) -> &[EcuRead] {
        &self.ecu_reads
    }
    pub fn rule_fields(&self) -> &[RuleFieldRead] {
        &self.rule_fields
    }
    pub fn component_values(&self) -> &[ComponentValue] {
        &self.component_values
    }
    pub fn ecu_reads_interrupted(&self) -> bool {
        self.ecu_reads_interrupted
    }
    pub fn entries(&self) -> &[EntryResult] {
        &self.entries
    }
    /// Whether the endpoint's bus and response identifier answered a presence request: an answer
    /// on a pair marks every endpoint with it, since alternatives often share one. Presence
    /// drops no candidate (ADR-327 item 2).
    pub fn answered(&self, endpoint: &EndpointKey) -> bool {
        self.presence
            .iter()
            .any(|answer| answer.bus == endpoint.bus && answer.rx_id == endpoint.rx_id)
    }
}

impl IdentificationReport {
    /// Sets each VIN rule's outcome from the established `vin`: matched when the compared
    /// positions equal the rule's expected text. It drops nobody itself; `contradictions` reads
    /// the outcomes (ADR-402).
    fn evaluate_vin_rules(&mut self, rules: &[(String, VinRule)], vin: &Vin) {
        for (result, (_, rule)) in self.vin_rules.iter_mut().zip(rules) {
            let compared = vin
                .as_str()
                .get(usize::from(rule.start - 1)..usize::from(rule.end));
            result.outcome = if compared == Some(rule.expected.as_str()) {
                RuleOutcome::Matched
            } else {
                RuleOutcome::NotMatched
            };
        }
    }
}

impl CheckedPlan {
    /// Derives the endpoints and presence groups (ADR-327 item 2, ADR-395 items 4 and 5).
    pub fn report(&self) -> IdentificationReport {
        let mut endpoints: Vec<EndpointEntry> = Vec::new();
        let mut buses: Vec<BusKey> = Vec::new();
        let mut groups: Vec<PresenceGroup> = Vec::new();
        for DocumentLink {
            document,
            link,
            functional_id,
        } in &self.links
        {
            let bus = BusKey::of(link);
            let key = EndpointKey {
                bus: bus.clone(),
                tx_id: link.tx_id,
                rx_id: link.rx_id,
            };
            match endpoints.iter_mut().find(|entry| entry.key == key) {
                Some(entry) => entry.documents.push(*document),
                None => endpoints.push(EndpointEntry {
                    key,
                    documents: vec![*document],
                }),
            }
            if !buses.contains(&bus) {
                buses.push(bus.clone());
            }
            if *functional_id != 0 {
                let known =
                    |g: &&mut PresenceGroup| g.bus == bus && g.functional_id == *functional_id;
                match groups.iter_mut().find(known) {
                    Some(group) => group.take_largest(link),
                    None => {
                        let mut group = PresenceGroup {
                            bus,
                            functional_id: *functional_id,
                            p2_max_ms: 0,
                            p2_star_ms: 0,
                            rc78_completion_ms: 0,
                            timings: Vec::new(),
                        };
                        group.take_largest(link);
                        groups.push(group);
                    }
                }
            }
        }
        IdentificationReport {
            selection: self.selection,
            candidates: self.candidates.clone(),
            remaining: self.candidates.clone(),
            endpoints,
            buses_without_presence: buses
                .into_iter()
                .filter(|bus| !groups.iter().any(|group| group.bus == *bus))
                .collect(),
            presence_groups: groups,
            refused_links: self.refused_links.clone(),
            presence: Vec::new(),
            presence_links_opened: 0,
            refused_tables: Vec::new(),
            presence_interrupted: false,
            vin: None,
            vins_seen: Vec::new(),
            vin_reads: self
                .vin_reads
                .iter()
                .map(|read| {
                    let outcome = if read.link.is_some() {
                        VinOutcome::NotRead
                    } else {
                        VinOutcome::LinkRefused
                    };
                    VinRead {
                        endpoint: read.endpoint.clone(),
                        request: read.request.clone(),
                        sources: read
                            .declarations
                            .iter()
                            .map(|(source, _)| VinSourceRead {
                                document: source.document,
                                service: source.service,
                                field: source.field,
                                outcome: outcome.clone(),
                            })
                            .collect(),
                        accepted: None,
                    }
                })
                .collect(),
            vin_interrupted: false,
            vehicle_lock_held: false,
            vin_rules: self
                .vin_rules
                .iter()
                .map(|(candidate, rule)| VinRuleResult {
                    candidate: candidate.clone(),
                    component: rule.component,
                    value: rule.value.clone(),
                    start: rule.start,
                    end: rule.end,
                    outcome: RuleOutcome::Unevaluated,
                })
                .collect(),
            rule_fields: self.rule_fields_initial(),
            component_values: Vec::new(),
            dropped: Vec::new(),
            read_order: Vec::new(),
            ecu_reads: self
                .ecu_reads
                .iter()
                .map(|read| EcuRead {
                    definition: read.definition.clone(),
                    endpoint: read.endpoint.clone(),
                    documents: read
                        .groups
                        .iter()
                        .flat_map(|group| group.documents.iter().copied())
                        .collect(),
                    requests: read
                        .groups
                        .iter()
                        .flat_map(|group| {
                            let outcome = if group.link.is_some() {
                                RequestOutcome::NotRead
                            } else {
                                RequestOutcome::LinkRefused
                            };
                            group
                                .requests
                                .iter()
                                .map(move |request| IdentificationRequest {
                                    request: request.request.clone(),
                                    declarations: request
                                        .declarations
                                        .iter()
                                        .map(|declaration| DeclarationOutcome {
                                            document: declaration.document,
                                            service: declaration.service,
                                            pattern: declaration.pattern,
                                            outcome: outcome.clone(),
                                        })
                                        .collect(),
                                })
                        })
                        .collect(),
                    matching: Vec::new(),
                    conclusive: false,
                    answered: false,
                })
                .collect(),
            ecu_reads_interrupted: false,
            entries: self
                .entries
                .iter()
                .map(|(candidate, entry)| EntryResult {
                    candidate: candidate.clone(),
                    role: entry.role.clone(),
                    optional: entry.optional,
                    alternatives: entry.alternatives.clone(),
                    state: EntryState::NotRead,
                })
                .collect(),
        }
    }

    /// The rule fields as the report starts with them: every source not read (or its link
    /// refused), no value established, every rule unevaluated (ADR-403).
    fn rule_fields_initial(&self) -> Vec<RuleFieldRead> {
        self.rule_fields
            .iter()
            .map(|field| {
                let read = &self.ecu_reads[field.read];
                let group = &read.groups[field.group];
                let outcome = if group.link.is_some() {
                    RuleSourceOutcome::NotRead
                } else {
                    RuleSourceOutcome::LinkRefused
                };
                RuleFieldRead {
                    definition: read.definition.clone(),
                    endpoint: read.endpoint.clone(),
                    group: field.group,
                    request: group.requests[field.request].request.clone(),
                    component: field.component,
                    sources: field
                        .sources
                        .iter()
                        .map(|source| RuleFieldSource {
                            document: source.document,
                            service: source.service,
                            field: source.field,
                            outcome: outcome.clone(),
                        })
                        .collect(),
                    established: None,
                    rules: self
                        .field_rules
                        .iter()
                        .filter(|(_, rule)| {
                            rule.component == field.component
                                && field.sources.contains(&rule.source)
                        })
                        .map(|(candidate, rule)| FieldRuleResult {
                            candidate: candidate.clone(),
                            value: rule.value.clone(),
                            document: rule.source.document,
                            service: rule.source.service,
                            field: rule.source.field,
                            expected: rule.expected.clone(),
                            outcome: RuleOutcome::Unevaluated,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// The link of `group`'s presence requests, before its receive table: the first declared
    /// link of the group with the group's largest timings (ADR-395 items 5 and 6).
    fn presence_link(&self, group: &PresenceGroup) -> LinkConfig {
        let first = self
            .links
            .iter()
            .find(|l| BusKey::of(&l.link) == group.bus && l.functional_id == group.functional_id)
            .expect("a presence group comes from a declared link");
        let mut link = first.link.clone();
        group.write_timings(&mut link);
        link
    }

    /// The physical request and response identifier of every endpoint on `bus`, in declaration
    /// order: the receive table of a presence link (ADR-395 item 6).
    fn bus_table(&self, bus: &BusKey) -> Vec<(u32, u32)> {
        let mut table: Vec<(u32, u32)> = Vec::new();
        for link in self.links.iter().filter(|l| BusKey::of(&l.link) == *bus) {
            let pair = (link.link.tx_id, link.link.rx_id);
            if !table.contains(&pair) {
                table.push(pair);
            }
        }
        table
    }
}

/// What an identification run returns: the report or the job's error, the guards, and how the
/// communication log ended. The result never depends on the log (ADR-347).
#[derive(Debug)]
pub struct IdentificationRun {
    pub result: Result<IdentificationReport, JobError>,
    pub guards: JobGuards,
    pub comm_log: CommLogStatus,
}

/// What the communication log's header and `job_start` say of an identification job: the facts
/// every run's log opens with (`runner::base_facts`, ADR-397), the kind and the plan's hash. The
/// job writes nothing and has no document of its own.
fn identification_facts(
    plan: &CheckedPlan,
    comm_log: Option<&CommLogSetup>,
    vci: Option<VciFacts>,
) -> RunFacts {
    RunFacts {
        plan_sha256: Some(plan.plan_sha256),
        ..base_facts(
            JobKind::Identification,
            comm_log.map(|setup| &setup.key),
            vci,
            VciMargins::default(),
        )
    }
}

/// Runs the plan's functional presence requests (ADR-327 item 2, ADR-396): per presence group, a
/// receive table of every declared pair on the group's bus, one link and one request at a time,
/// under the per-VCI lock `guards` hold. A table the VCI refuses (the worker's answer to the
/// table or the connect, never a timeout or a lost path) is halved in declaration order, each
/// part getting a link and a request of its own; a single pair still refused refuses the job.
/// When the path to the VCI is lost, the link is opened again within `limits` (ADR-393, ADR-394)
/// and the request sent again within `limits.max_primitive_repeats` with the origin `repeat`
/// (ADR-392); what was heard before the loss stays, and a run whose bounds run out reports
/// `presence_interrupted`. A cancel (`cancel`, also set when the future is dropped) ends the run
/// in [`JobError::Cancelled`]. `source` also gives the clients of the later links. `vci` is what
/// the agent knows of the VCI the worker was launched for, for the log's `vci_connected`.
///
/// Then, unless the presence phase was interrupted, it reads the VIN once per read of the plan's
/// VIN rules (ADR-399): a link and a read-only request at a time, under the same bounds. The VIN
/// is established when the reads that gave a well-formed VIN gave one VIN; the run then takes the
/// per-vehicle lock of it on `guards`, with no link open, waiting for a job that holds it until
/// the cancel, and reports how each VIN rule stands to it. The VIN is in the report and the
/// communication log only. Last, unless the VIN phase was interrupted, it reads each ECU
/// definition of the plan (ADR-401) the same way, with the lock held, and reports the entry
/// states; guards still marked end the run in [`JobError::LinkUnconfirmed`] when a read remains.
pub async fn run_identification<W: WorkerSource>(
    mut source: W,
    plan: CheckedPlan,
    limits: JobLimits,
    guards: JobGuards,
    comm_log: Option<CommLogSetup>,
    vci: Option<VciFacts>,
    cancel: Arc<AtomicBool>,
) -> IdentificationRun {
    let handle = Handle::current();
    let slot: GuardSlot = Arc::new(Mutex::new(Some(guards)));
    let refused = |slot: &GuardSlot, error: JobError| {
        let guards = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        IdentificationRun {
            result: Err(error),
            guards: guards.expect("nothing else takes the guards out of the slot"),
            comm_log: CommLogStatus::None,
        }
    };
    if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
        return refused(&slot, JobError::CurrentThreadRuntime);
    }
    if link_unconfirmed(&slot) {
        return refused(&slot, JobError::LinkUnconfirmed);
    }
    // A worker that cannot give its first client ends the job before any link or log.
    if let Err(error) = source.client() {
        return refused(&slot, JobError::Link(error));
    }
    let log = JobLog::new(
        comm_log.clone(),
        identification_facts(&plan, comm_log.as_ref(), vci),
    );
    let cancelled = CancelOnDrop(Arc::clone(&cancel));
    let job_slot = Arc::clone(&slot);
    let (result, comm_log) = on_job_thread(
        log,
        move |log| {
            identify(
                &handle,
                Arc::new(Mutex::new(source)),
                &plan,
                &limits,
                &cancel,
                &job_slot,
                log,
            )
        },
        |log, result| {
            log.finish_as(match result {
                Ok(_) => "ok",
                Err(error) => crate::comm_log::error_outcome_name(error),
            })
        },
    )
    .await;
    drop(cancelled);
    let guards = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    IdentificationRun {
        result,
        guards: guards.expect("nothing else takes the guards out of the slot"),
        comm_log,
    }
}

/// Settles the worker after a link failed to open, or before one opens (`evidence` empty), by
/// the rule a host follows when it opens a link again (ADR-394, [`settle_worker`]): a worker that
/// exited or was ended is logged and the guards' mark cleared, since it holds no link (ADR-258
/// item 3), and the next client is a restart. Returns whether the worker was found gone.
fn settle(
    source: &mut impl WorkerSource,
    evidence: CloseEvidence,
    comm_log: &Option<crate::comm_log::CommLog>,
    guards: &GuardSlot,
    worker_ended: &Cell<bool>,
) -> bool {
    if worker_ended.get()
        || !settle_worker(source, evidence, WORKER_EXIT_GRACE, comm_log.as_ref(), &[])
    {
        return false;
    }
    worker_ended.set(true);
    if let Some(held) = guards.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        held.worker_gone();
    }
    true
}

/// Whether the guards in `slot` are marked: the worker may still hold a link (ADR-258).
fn link_unconfirmed(slot: &GuardSlot) -> bool {
    let held = slot.lock().unwrap_or_else(|e| e.into_inner());
    held.as_ref().is_some_and(JobGuards::link_unconfirmed)
}

/// A worker source shared by the links of one run: each link gets its client from the one source,
/// which also restarts a worker that exited (ADR-394).
struct SharedSource<W: WorkerSource>(Arc<Mutex<W>>);

impl<W: WorkerSource> SharedSource<W> {
    fn with<T>(&self, call: impl FnOnce(&mut W) -> T) -> T {
        call(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl<W: WorkerSource> WorkerSource for SharedSource<W> {
    type Client = W::Client;
    fn client(&mut self) -> Result<Self::Client, HostError> {
        self.with(W::client)
    }
    fn status(&mut self) -> WorkerStatus {
        self.with(W::status)
    }
    fn end(&mut self) -> Result<WorkerExit, HostError> {
        self.with(W::end)
    }
    fn pid(&self) -> Option<u32> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).pid()
    }
}

/// How one part of a group's table ended.
enum PartEnd {
    /// The window ended; the answers heard.
    Done(Vec<(u32, Vec<u8>)>),
    /// The worker answered, refusing the table or the connect, with its error (ADR-396).
    Refused(JobError),
    /// The path to the VCI was lost and the bounds ran out; the answers heard before, and
    /// whether a link had opened.
    Interrupted(Vec<(u32, Vec<u8>)>, bool),
}

/// The run on the job's blocking thread.
fn identify<W: WorkerSource>(
    handle: &Handle,
    source: Arc<Mutex<W>>,
    plan: &CheckedPlan,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    log: &mut JobLog,
) -> Result<IdentificationReport, JobError> {
    // The guards stay held until this function returns, after every link is closed.
    let _guards = Arc::clone(guards);
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let mut report = plan.report();
    let comm_log = log.open(RunKind::First);
    let mut reopens = 0u8;
    let worker_ended = Cell::new(false);
    'groups: for group in report.presence_groups.clone() {
        let base = plan.presence_link(&group);
        let mut queue = VecDeque::from([plan.bus_table(&group.bus)]);
        while let Some(table) = queue.pop_front() {
            let config = base.functional(group.functional_id, table.clone());
            let part = presence_part(
                handle,
                &source,
                &config,
                comm_log.clone(),
                limits,
                cancelled,
                guards,
                &mut reopens,
                &worker_ended,
            )?;
            let (heard, interrupted, opened) = match part {
                PartEnd::Done(heard) => (heard, false, true),
                PartEnd::Interrupted(heard, opened) => (heard, true, opened),
                PartEnd::Refused(error) => {
                    // The halves go to the front, the first half first, so one link is open at a
                    // time and the order stays the declaration order (ADR-395 item 6). A single
                    // pair that is still refused refuses the job.
                    let [first, second] = halves(&table).ok_or(error)?;
                    report.refused_tables.push(RefusedTable {
                        bus: group.bus.clone(),
                        functional_id: group.functional_id,
                        table,
                    });
                    queue.push_front(second);
                    queue.push_front(first);
                    continue;
                }
            };
            report.presence_links_opened += u32::from(opened);
            for (rx_id, bytes) in heard {
                let answer = PresenceAnswer {
                    bus: group.bus.clone(),
                    rx_id,
                    bytes,
                };
                if !report.presence.contains(&answer) {
                    report.presence.push(answer);
                }
            }
            if interrupted {
                report.presence_interrupted = true;
                break 'groups;
            }
        }
    }
    if report.presence_interrupted {
        // The run's bounds ran out: no VIN link opens, and nothing is established (ADR-399).
        report.vin_interrupted = plan.vin_reads.iter().any(|read| read.link.is_some());
        report.ecu_reads_interrupted = plan.ecu_reads_to_do();
        return Ok(report);
    }
    vin_phase(
        handle,
        &source,
        plan,
        limits,
        cancelled,
        guards,
        &comm_log,
        &mut reopens,
        &worker_ended,
        &mut report,
    )?;
    if report.vin_interrupted {
        // The bounds ran out in a VIN read: nothing is established, and every later link would
        // fail the same way (ADR-401).
        report.ecu_reads_interrupted = plan.ecu_reads_to_do();
        return Ok(report);
    }
    if let Some(vin) = report.vin.clone() {
        report.evaluate_vin_rules(&plan.vin_rules, &vin);
        // Every VIN link is closed here, so the wait for a vehicle another job holds keeps no
        // VCI link open (design 8.8.1, ADR-399 item 4). The promotion settles the worker as an
        // open does: a worker that exited during the last close is found, and its mark cleared.
        // Guards still marked take no lock (ADR-258); an ECU read that remains then ends the run
        // in `LinkUnconfirmed` as its link opens (ADR-401), and a run with none reports so.
        match settle_before_open(&source, &comm_log, cancelled, guards, &worker_ended) {
            Ok(_) => {
                promote_to_vehicle(
                    guards,
                    &vin,
                    limits.wait_poll.max(std::time::Duration::from_millis(1)),
                    cancelled,
                )?;
                report.vehicle_lock_held = true;
            }
            Err(JobError::LinkUnconfirmed) => {}
            Err(error) => return Err(error),
        }
    }
    ecu_phase(
        handle,
        &source,
        plan,
        limits,
        cancelled,
        guards,
        &comm_log,
        &mut reopens,
        &worker_ended,
        &mut report,
    )?;
    // An interrupted phase leaves every entry not read, as an interrupted VIN phase establishes
    // nothing (ADR-401).
    if !report.ecu_reads_interrupted {
        evaluate_entries(&mut report.entries, &report.ecu_reads);
    }
    // A matched rule is a fact, whether or not the phase ran to its end.
    report.component_values = component_values(&report);
    Ok(report)
}

/// A table cut in two, the first half the larger; `None` for a single pair.
fn halves(table: &[(u32, u32)]) -> Option<[Vec<(u32, u32)>; 2]> {
    (table.len() > 1).then(|| {
        let (first, second) = table.split_at(table.len().div_ceil(2));
        [first.to_vec(), second.to_vec()]
    })
}

/// What every open of a link, and the promotion to the vehicle's lock after the last one, does
/// first: a cancel, a worker that exited and the guards' mark are judged, so a link the worker may
/// still hold ends the job (ADR-258, ADR-394). Gives the source for the open, or
/// [`JobError::Cancelled`], or [`JobError::LinkUnconfirmed`] when the guards are still marked.
fn settle_before_open<W: WorkerSource>(
    source: &Arc<Mutex<W>>,
    comm_log: &Option<crate::comm_log::CommLog>,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    worker_ended: &Cell<bool>,
) -> Result<SharedSource<W>, JobError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    let mut shared = SharedSource(Arc::clone(source));
    settle(
        &mut shared,
        CloseEvidence::default(),
        comm_log,
        guards,
        worker_ended,
    );
    if link_unconfirmed(guards) {
        return Err(JobError::LinkUnconfirmed);
    }
    Ok(shared)
}

/// How [`open_part`] ended without an open link.
enum Opened<C: WorkerApi> {
    /// The link is open.
    Link(Box<JobLink<C>>),
    /// The worker answered, refusing the table or the connect, with its error (ADR-396).
    Refused(JobError),
    /// The link failed to open for a reason that is neither a lost path nor the table, and
    /// what was opened is closed again. The presence phase ends the job in it; a VIN read
    /// reports its sources, and an ECU read its declarations, as refused by the worker (ADR-399
    /// item 3, ADR-401).
    Failed(JobError),
    /// The path to the VCI was lost and the bounds ran out before the link opened.
    Interrupted,
}

/// Opens the link of `config`: before each try a cancel, a worker that exited and the guards'
/// mark are judged (ADR-258), and a link that fails to open on a lost path is opened again
/// within the run's bounds (ADR-393). A table the worker refused is [`Opened::Refused`]; any
/// other failure to open ends the job. The presence parts, the VIN reads and the ECU reads open
/// their links so.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn open_part<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    config: &LinkConfig,
    comm_log: Option<crate::comm_log::CommLog>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
) -> Result<Opened<W::Client>, JobError> {
    let timings = Timings::for_link(config);
    let try_open = || {
        let stop = |error| LinkRefused {
            error,
            cleaned_up: true,
            table_refused: false,
            evidence: CloseEvidence::default(),
        };
        let mut shared =
            settle_before_open(source, &comm_log, cancelled, guards, worker_ended).map_err(stop)?;
        let client = shared
            .client()
            .map_err(|error| stop(JobError::Link(error)))?;
        if worker_ended.replace(false)
            && let Some(log) = &comm_log
        {
            log.worker_restart(shared.pid());
        }
        let opened = open_job_link(
            handle,
            client,
            Box::new(SharedSource(Arc::clone(source))),
            config,
            None,
            None,
            comm_log.clone(),
            guards,
            timings,
        );
        // An open that failed on a lost path and could not close what it opened may have been
        // cut by the worker's exit, which confirms the link closed (ADR-394): a worker found
        // exited is logged and the mark cleared, so the open is tried again.
        opened.map_err(|mut refused| {
            let lost = matches!(&refused.error, JobError::Link(error)
                if error.resend_class() == ResendClass::PathLost);
            if !refused.cleaned_up && lost && refused.evidence.silent() {
                refused.cleaned_up = settle(
                    &mut shared,
                    refused.evidence,
                    &comm_log,
                    guards,
                    worker_ended,
                );
            }
            refused
        })
    };
    let mut opened = try_open();
    if opened.as_ref().is_err_and(is_path_loss) {
        let mut last = None;
        let mut link = None;
        reopen_attempts(
            || match try_open() {
                Ok(open) => {
                    link = Some(open);
                    ReopenAttempt::Opened
                }
                Err(refused) if is_path_loss(&refused) => ReopenAttempt::NotYet,
                Err(refused) => {
                    last = Some(refused);
                    ReopenAttempt::Cannot
                }
            },
            limits,
            cancelled,
            reopens,
        )?;
        opened = match (link, last) {
            (Some(link), _) => Ok(link),
            (None, Some(refused)) => Err(refused),
            // The VCI did not come back within the bounds.
            (None, None) => return Ok(Opened::Interrupted),
        };
    }
    match opened {
        Ok(link) => Ok(Opened::Link(Box::new(link))),
        // A refusal whose teardown failed left the guards marked: nothing more opens (ADR-258).
        Err(refused) if refused.table_refused && refused.cleaned_up => {
            Ok(Opened::Refused(refused.error))
        }
        // The worker answered or the call failed, and nothing is left open (a worker that cannot
        // be started, a cancel and a mark on the guards are the job's end).
        Err(refused)
            if refused.cleaned_up
                && matches!(&refused.error, JobError::Link(error)
                    if !matches!(error, HostError::WorkerStart { .. })) =>
        {
            Ok(Opened::Failed(refused.error))
        }
        Err(refused) => Err(refused.error),
    }
}

/// How [`repeat_within_bounds`] ended.
enum Repeated {
    /// An attempt succeeded.
    Done,
    /// The attempts could not complete within the run's bounds, or a failure ended them; the
    /// last error.
    Stopped(HostError),
}

/// Makes `attempt` on the open link of `host` until it succeeds, sending it again after a
/// failure within the run's bounds (ADR-392, ADR-393): after an ECU-level or transient failure on
/// the same link, after a lost path on a link opened again, each at most
/// `limits.max_primitive_repeats` times, with the origin `repeat`; the reopens count over the
/// run in `reopens` (ADR-396 item 6). `ends_run` tells an error that stops the attempts at once.
/// A cancel is [`JobError::Cancelled`]; an error of no class that a resend fits ends the job. The
/// presence request, the VIN read and the ECU read send so (ADR-399, ADR-401).
fn repeat_within_bounds<C: WorkerApi>(
    host: &mut WorkerHost<C>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    reopens: &mut u8,
    mut attempt: impl FnMut(&mut WorkerHost<C>) -> Result<(), HostError>,
    ends_run: impl Fn(&HostError) -> bool,
) -> Result<Repeated, JobError> {
    let mut attempts = 0u8;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        // The origin is the identification's, and `repeat` for a request sent again (ADR-392).
        host.own_reads(Some(OwnRead::Identification));
        if attempts > 0 {
            host.step_resends();
        }
        let end = attempt(host);
        host.step_ends();
        host.own_reads(None);
        let Err(error) = end else {
            return Ok(Repeated::Done);
        };
        if error.resend_class() == ResendClass::PathLost {
            host.path_lost();
        }
        let again = attempts < limits.max_primitive_repeats;
        if ends_run(&error) {
            return Ok(Repeated::Stopped(error));
        }
        match error.resend_class() {
            // The ECU, the bus or the events failed the exchange, or the start was refused for a
            // reason that passes: the same link carries the request again.
            ResendClass::EcuLevel | ResendClass::Transient if again => {
                std::thread::sleep(limits.wait_poll.max(std::time::Duration::from_millis(1)));
            }
            ResendClass::PathLost if again => {
                if !reopen_attempts(|| host.reopen_link(), limits, cancelled, reopens)? {
                    return Ok(Repeated::Stopped(error));
                }
            }
            ResendClass::EcuLevel | ResendClass::PathLost | ResendClass::Transient => {
                return Ok(Repeated::Stopped(error));
            }
            ResendClass::Other => return Err(JobError::Link(error)),
        }
        attempts += 1;
    }
}

/// Opens the link of `config`, sends the presence request on it until a window ends, and closes
/// it. A link that fails to open on a lost path is opened again within the run's bounds; a
/// table the worker refused is [`PartEnd::Refused`]; any other failure to open ends the job.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn presence_part<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    config: &LinkConfig,
    comm_log: Option<crate::comm_log::CommLog>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
) -> Result<PartEnd, JobError> {
    let link = match open_part(
        handle,
        source,
        config,
        comm_log,
        limits,
        cancelled,
        guards,
        reopens,
        worker_ended,
    )? {
        Opened::Link(link) => link,
        Opened::Refused(error) => return Ok(PartEnd::Refused(error)),
        Opened::Failed(error) => return Err(error),
        Opened::Interrupted => return Ok(PartEnd::Interrupted(Vec::new(), false)),
    };
    let JobLink {
        mut host,
        open_link,
        path_lost,
        ..
    } = *link;
    let mut heard = Vec::new();
    let ended = repeat_within_bounds(
        &mut host,
        limits,
        cancelled,
        reopens,
        |host| {
            let presence = host.presence(&PRESENCE_REQUEST);
            heard.extend(presence.answers);
            presence.end
        },
        // A receive error that ended the window is not a clean window, and sending the request
        // again would not tell more: the part is interrupted with its answers (ADR-396).
        |error| matches!(error, HostError::PrimitiveFailed(_)),
    );
    close_job_link(host, open_link, &path_lost);
    Ok(match ended? {
        Repeated::Done => PartEnd::Done(heard),
        Repeated::Stopped(_) => PartEnd::Interrupted(heard, true),
    })
}

/// How [`open_read_link`] ended without an open link.
enum ReadLink<C: WorkerApi> {
    /// The link is open.
    Link(Box<JobLink<C>>),
    /// The worker refused to open the link (a connect it refused, or another failure to open that
    /// left nothing open) when the read was made.
    Refused,
    /// The path to the VCI was lost and the bounds ran out before the link opened.
    Interrupted,
}

/// Opens the link of a read (a link ADR-324 accepts, not a table) by [`open_part`] and maps its
/// end: a refusal or another failure that left nothing open is [`ReadLink::Refused`]. The VIN
/// reads and the ECU reads open their links so (ADR-399 item 3, ADR-401).
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn open_read_link<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    link: &LinkConfig,
    comm_log: Option<crate::comm_log::CommLog>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
) -> Result<ReadLink<W::Client>, JobError> {
    Ok(
        match open_part(
            handle,
            source,
            link,
            comm_log,
            limits,
            cancelled,
            guards,
            reopens,
            worker_ended,
        )? {
            Opened::Link(link) => ReadLink::Link(link),
            Opened::Refused(_) | Opened::Failed(_) => ReadLink::Refused,
            Opened::Interrupted => ReadLink::Interrupted,
        },
    )
}

/// How [`identification_request`] ended.
enum RequestEnd {
    /// The ECU's response, positive or negative.
    Response(Vec<u8>),
    /// No usable answer within the repeats: none at all, the link's error event for the request,
    /// a receive error or lost events (each a failure of the ECU or the bus, ADR-392).
    NoUsableAnswer,
    /// The path to the VCI was lost and the bounds ran out, or a failure of another class
    /// stopped the attempts.
    Interrupted,
}

/// Sends the read-only `request` (SID first) on the open link of `host` and again within the run's
/// bounds, as [`repeat_within_bounds`] has it (a read is idempotent, so a receive error sends it
/// again too, ADR-392 item 2). The VIN reads and the ECU reads send so (ADR-399, ADR-401).
fn identification_request<C: WorkerApi>(
    host: &mut WorkerHost<C>,
    request: &[u8],
    limits: &JobLimits,
    cancelled: &AtomicBool,
    reopens: &mut u8,
) -> Result<RequestEnd, JobError> {
    let (sid, payload) = request
        .split_first()
        .expect("an identification request has a service identifier");
    let mut response = None;
    let ended = repeat_within_bounds(
        host,
        limits,
        cancelled,
        reopens,
        |host| {
            response = Some(host.service_request(u16::from(*sid), payload)?);
            Ok(())
        },
        |_| false,
    )?;
    Ok(match (ended, response) {
        (Repeated::Done, Some(response)) => RequestEnd::Response(response),
        (Repeated::Stopped(error), _) if error.resend_class() == ResendClass::EcuLevel => {
            RequestEnd::NoUsableAnswer
        }
        _ => RequestEnd::Interrupted,
    })
}

/// How one VIN read ended.
enum VinReadEnd {
    /// The read got an answer, or none within the repeats: what each source gave, and the VIN
    /// when they all gave the same well-formed one.
    Done(Vec<VinOutcome>, Option<Vin>),
    /// The worker refused to open the link.
    Refused,
    /// The link could not open, or the request not be sent, within the run's bounds.
    Interrupted,
}

/// Makes one VIN read: opens the link of `read` (a link ADR-324 accepts, not a table), sends the
/// request once and again within the run's bounds, decodes the one response with every source
/// of the read and closes the link (ADR-399 item 1). A read is idempotent, so a receive error
/// sends it again too (ADR-392 item 2). The VIN goes to the communication log's `identity`
/// event, and nowhere else.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn vin_read<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    read: &VinReadPlan,
    link: &LinkConfig,
    comm_log: Option<crate::comm_log::CommLog>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
) -> Result<VinReadEnd, JobError> {
    let link = match open_read_link(
        handle,
        source,
        link,
        comm_log,
        limits,
        cancelled,
        guards,
        reopens,
        worker_ended,
    )? {
        ReadLink::Link(link) => link,
        // The worker refused the link at run time: its sources are reported so and the other
        // reads decide (ADR-399 item 3).
        ReadLink::Refused => return Ok(VinReadEnd::Refused),
        ReadLink::Interrupted => return Ok(VinReadEnd::Interrupted),
    };
    let JobLink {
        mut host,
        open_link,
        path_lost,
        ..
    } = *link;
    let (sid, _) = read
        .request
        .split_first()
        .expect("a VIN source's request has a service identifier");
    let ended = identification_request(&mut host, &read.request, limits, cancelled, reopens);
    let end = ended.map(|ended| match ended {
        RequestEnd::Response(response) => {
            let outcomes: Vec<VinOutcome> = read
                .declarations
                .iter()
                .map(|(_, field)| {
                    let bytes = field_bytes_of(field, *sid, &response);
                    host.note_identity(IdentityKind::Vin, identity_read_of(&bytes));
                    vin_outcome(bytes)
                })
                .collect();
            let accepted = accepted_vin(&outcomes);
            VinReadEnd::Done(outcomes, accepted)
        }
        RequestEnd::NoUsableAnswer => VinReadEnd::Done(
            vec![VinOutcome::NoUsableAnswer; read.declarations.len()],
            None,
        ),
        RequestEnd::Interrupted => VinReadEnd::Interrupted,
    });
    close_job_link(host, open_link, &path_lost);
    end
}

/// What a source gave, from the bytes its field was read as.
fn vin_outcome(read: FieldBytes) -> VinOutcome {
    match read {
        FieldBytes::Field(bytes) => match String::from_utf8(bytes) {
            Ok(text) if Vin::is_well_formed_text(&text) => VinOutcome::Decoded(Vin::new(text)),
            _ => VinOutcome::NotWellFormed,
        },
        FieldBytes::Negative(nrc) => VinOutcome::Negative(nrc),
        FieldBytes::Unreadable => VinOutcome::Undecodable,
    }
}

/// The VIN of a read when every source gave the same well-formed one (ADR-399 item 1).
fn accepted_vin(outcomes: &[VinOutcome]) -> Option<Vin> {
    agreed(outcomes.iter().map(|outcome| match outcome {
        VinOutcome::Decoded(vin) => Some(vin.clone()),
        _ => None,
    }))
}

/// The one value every item gave: `None` when there is no item, an item gave none or two differ.
/// What "every source gave the same" means for a VIN read and for a rule field (ADR-399 item 1,
/// ADR-403).
fn agreed<T: PartialEq>(items: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    let mut agreed = None;
    for item in items {
        let item = item?;
        match &agreed {
            Some(known) if *known != item => return None,
            _ => agreed = Some(item),
        }
    }
    agreed
}

/// The VIN phase: reads the VIN once per read of the plan, in plan order, one link open at a time,
/// and sets the report's VIN section (ADR-399). The VIN is established when at least one read
/// accepted one and every accepted VIN is the same; reads that gave nothing do not block it. A
/// run whose bounds run out in a read leaves the remaining reads `NotRead` and establishes
/// nothing. Every link is closed when this returns.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn vin_phase<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    plan: &CheckedPlan,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    comm_log: &Option<crate::comm_log::CommLog>,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
    report: &mut IdentificationReport,
) -> Result<(), JobError> {
    for (at, read) in plan.vin_reads.iter().enumerate() {
        let Some(link) = &read.link else {
            continue;
        };
        let end = vin_read(
            handle,
            source,
            read,
            link,
            comm_log.clone(),
            limits,
            cancelled,
            guards,
            reopens,
            worker_ended,
        )?;
        let (outcomes, accepted) = match end {
            VinReadEnd::Done(outcomes, accepted) => (outcomes, accepted),
            VinReadEnd::Refused => (
                vec![VinOutcome::WorkerRefused; read.declarations.len()],
                None,
            ),
            VinReadEnd::Interrupted => {
                report.vin_interrupted = true;
                break;
            }
        };
        let reported = &mut report.vin_reads[at];
        for (source, outcome) in reported.sources.iter_mut().zip(outcomes) {
            source.outcome = outcome;
        }
        if let Some(vin) = &accepted
            && !report.vins_seen.contains(vin)
        {
            report.vins_seen.push(vin.clone());
        }
        reported.accepted = accepted;
    }
    if !report.vin_interrupted && report.vins_seen.len() == 1 {
        report.vin = report.vins_seen.first().cloned();
    }
    Ok(())
}

/// What the requests of one link group gave.
#[derive(Default)]
struct GroupRead {
    /// Per request in group order, the outcome of each declaration; fewer than the group's
    /// requests when the bounds ran out.
    outcomes: Vec<Vec<RequestOutcome>>,
    /// The fields each declaration that decoded gave, for the variants' patterns.
    decoded: HashMap<(Sha256, u32), Vec<DecodedField>>,
    /// The path to the VCI was lost and the bounds ran out before the group was read.
    interrupted: bool,
}

/// What each declaration of a request gave from its one `response` (ADR-401): a negative
/// response to the request, else the response decoded through the declaration's service.
fn declaration_outcomes(
    request: &RequestPlan,
    sid: u8,
    response: &[u8],
    documents: &[(Sha256, JobDocument)],
    read: &mut GroupRead,
) -> Vec<RequestOutcome> {
    request
        .declarations
        .iter()
        .map(
            |&Declaration {
                 document: hash,
                 service,
                 ..
             }| {
                if let Some(nrc) = negative_code(sid, response) {
                    return RequestOutcome::Negative(nrc);
                }
                let Some((_, document)) = documents.iter().find(|(known, _)| *known == hash) else {
                    return RequestOutcome::Undecodable;
                };
                match decode_identification_answer(&document.variant(), service, response) {
                    Ok(Some(fields)) => {
                        let outcome = RequestOutcome::Decoded(fields_of(&fields));
                        read.decoded.insert((hash, service), fields);
                        outcome
                    }
                    Ok(None) | Err(_) => RequestOutcome::Undecodable,
                }
            },
        )
        .collect()
}

/// Reads one link group: opens its link, sends each request once and again within the run's
/// bounds, decodes the one response of each with every declaration that shares it, and closes the
/// link (ADR-401). A read is idempotent, so a receive error sends it again too (ADR-392 item 2).
/// `None` when the worker refused to open the link.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn ecu_read_group<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    group: &LinkGroup,
    link: &LinkConfig,
    documents: &[(Sha256, JobDocument)],
    comm_log: Option<crate::comm_log::CommLog>,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
) -> Result<Option<GroupRead>, JobError> {
    let link = match open_read_link(
        handle,
        source,
        link,
        comm_log,
        limits,
        cancelled,
        guards,
        reopens,
        worker_ended,
    )? {
        ReadLink::Link(link) => link,
        // The worker refused the link at run time: its declarations are reported so and the
        // other groups decide (ADR-399 item 3).
        ReadLink::Refused => return Ok(None),
        ReadLink::Interrupted => {
            return Ok(Some(GroupRead {
                interrupted: true,
                ..GroupRead::default()
            }));
        }
    };
    let JobLink {
        mut host,
        open_link,
        path_lost,
        ..
    } = *link;
    let mut read = GroupRead::default();
    let mut failure = None;
    for request in &group.requests {
        let outcomes =
            match identification_request(&mut host, &request.request, limits, cancelled, reopens) {
                Ok(RequestEnd::Response(response)) => {
                    let sid = request.request[0];
                    declaration_outcomes(request, sid, &response, documents, &mut read)
                }
                Ok(RequestEnd::NoUsableAnswer) => {
                    vec![RequestOutcome::NoUsableAnswer; request.declarations.len()]
                }
                Ok(RequestEnd::Interrupted) => {
                    read.interrupted = true;
                    break;
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
        read.outcomes.push(outcomes);
    }
    close_job_link(host, open_link, &path_lost);
    failure.map_or(Ok(Some(read)), Err)
}

/// What one reading of a definition's endpoint established (ADR-327 item 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// A variant of the definition matched there.
    Identified,
    /// The read was conclusive and no variant matched.
    RuledOut,
}

/// A definition's verdict at one endpoint.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Fact {
    definition: String,
    endpoint: EndpointKey,
    verdict: Verdict,
}

/// What the reads established (ADR-327 item 3): a read with a match identifies its definition at
/// its endpoint; a conclusive read without a match rules it out there. A read without an endpoint
/// (a refused link) gives no fact, nor does one that was not made or stayed inconclusive.
/// Callers: `narrow`, `next_definition`.
fn facts_of(reads: &[EcuRead]) -> Vec<Fact> {
    reads
        .iter()
        .filter_map(|read| {
            let verdict = if !read.matching.is_empty() {
                Verdict::Identified
            } else if read.conclusive {
                Verdict::RuledOut
            } else {
                return None;
            };
            Some(Fact {
                definition: read.definition.clone(),
                endpoint: read.endpoint.clone()?,
                verdict,
            })
        })
        .collect()
}

/// Whether some group of the planned read has a request a variant pattern names: a read for a
/// field rule's source alone is no identification request (ADR-327 item 3, ADR-403).
fn has_request(read: &EcuReadPlan) -> bool {
    read.groups.iter().any(|group| {
        group
            .requests
            .iter()
            .any(|request| request.declarations.iter().any(|d| d.pattern))
    })
}

/// Whether a variant of the definition has a link ADR-324 refuses (a read without an endpoint
/// that has a request): the definition is inconclusive at every endpoint, so no read rules it
/// out (ADR-401 item 4). Callers: `read_definition_at`, `outcome_sets`, `contradictions`.
fn has_refused_variant(plan: &CheckedPlan, definition: &str) -> bool {
    plan.ecu_reads
        .iter()
        .any(|read| read.definition == definition && read.endpoint.is_none() && has_request(read))
}

/// Every contradiction the facts and the VIN rules hold against each candidate, in plan order,
/// only for the candidates that have one (ADR-327 item 3, ADR-402). A candidate is contradicted by
/// an identified definition it lists in no entry, at an endpoint one of its entries uses, once
/// every definition it lists there is ruled out there (so one whose read stayed inconclusive,
/// failed or got no answer keeps it); a fact of a definition that has left play still counts. And
/// per component, by its VIN rules when the VIN is established and none of them matched; an
/// unevaluated rule contradicts nothing; and per component and rule field, by its field rules when
/// the field is established and none of them matched (ADR-403). The one function the run (`narrow`) and the hypothetical
/// outcomes (`outcome_sets`) both judge by, so the choice and the drop cannot drift apart.
fn contradictions(
    plan: &CheckedPlan,
    facts: &[Fact],
    vin_rules: &[VinRuleResult],
    rule_fields: &[RuleFieldRead],
) -> Vec<(String, Vec<Contradiction>)> {
    let mut found = Vec::new();
    for candidate in &plan.candidates {
        let mut contradictions = Vec::new();
        let rules = || vin_rules.iter().filter(|rule| rule.candidate == *candidate);
        let mut components: Vec<Component> = Vec::new();
        for rule in rules() {
            if !components.contains(&rule.component) {
                components.push(rule.component);
            }
        }
        for component in components {
            if rules()
                .filter(|rule| rule.component == component)
                .all(|rule| rule.outcome == RuleOutcome::NotMatched)
            {
                contradictions.push(Contradiction::VinRules { component });
            }
        }
        // Per component and rule field: an established field none of whose rules of the
        // candidate matched; another field of the same component neither saves nor drops it, and
        // an unestablished field drops nothing (ADR-403).
        for field in rule_fields {
            let (Some(_), Some(endpoint)) = (&field.established, &field.endpoint) else {
                continue;
            };
            let own = || {
                field
                    .rules
                    .iter()
                    .filter(|rule| rule.candidate == *candidate)
            };
            if own().next().is_some() && own().all(|rule| rule.outcome != RuleOutcome::Matched) {
                contradictions.push(Contradiction::FieldRules {
                    component: field.component,
                    definition: field.definition.clone(),
                    endpoint: endpoint.clone(),
                    group: field.group,
                    request: field.request.clone(),
                });
            }
        }
        let listed: Vec<&String> = plan
            .entries
            .iter()
            .filter(|(owner, _)| owner == candidate)
            .flat_map(|(_, entry)| &entry.alternatives)
            .collect();
        for fact in facts.iter().filter(|fact| {
            fact.verdict == Verdict::Identified && !listed.contains(&&fact.definition)
        }) {
            let mut at: Vec<String> = Vec::new();
            for read in &plan.ecu_reads {
                if read.endpoint.as_ref() == Some(&fact.endpoint)
                    && has_request(read)
                    && listed.contains(&&read.definition)
                    && !at.contains(&read.definition)
                {
                    at.push(read.definition.clone());
                }
            }
            // A listed definition with a refused variant is listed at every endpoint the
            // candidate uses and is never ruled out, so it keeps the candidate (ADR-401 item 4).
            if !at.is_empty() {
                for definition in &listed {
                    if has_refused_variant(plan, definition) && !at.contains(definition) {
                        at.push((*definition).clone());
                    }
                }
            }
            let ruled_out = |definition: &String| {
                facts.iter().any(|other| {
                    other.verdict == Verdict::RuledOut
                        && other.definition == *definition
                        && other.endpoint == fact.endpoint
                })
            };
            if !at.is_empty() && at.iter().all(ruled_out) {
                contradictions.push(Contradiction::UnlistedDefinition {
                    definition: fact.definition.clone(),
                    endpoint: fact.endpoint.clone(),
                    ruled_out: at,
                });
            }
        }
        if !contradictions.is_empty() {
            found.push((candidate.clone(), contradictions));
        }
    }
    found
}

/// Recomputes the remaining candidates from the facts and the VIN rules, records each newly
/// contradicted candidate in `report.dropped` with `after`, and writes `report.remaining`. A plan
/// whose selection does not narrow keeps every candidate and records nothing (ADR-327 item 8).
/// Callers: `ecu_phase`, before the first read and after each definition.
fn narrow(
    plan: &CheckedPlan,
    report: &mut IdentificationReport,
    after: Option<&str>,
) -> Vec<String> {
    if !plan.selection.narrows() {
        return report.remaining.clone();
    }
    let facts = facts_of(&report.ecu_reads);
    let contradicted = contradictions(plan, &facts, &report.vin_rules, &report.rule_fields);
    for (candidate, contradictions) in &contradicted {
        if !report.dropped.iter().any(|d| d.candidate == *candidate) {
            report.dropped.push(DroppedCandidate {
                candidate: candidate.clone(),
                after: after.map(str::to_owned),
                contradictions: contradictions.clone(),
            });
        }
    }
    report.remaining = plan
        .candidates
        .iter()
        .filter(|candidate| {
            !contradicted
                .iter()
                .any(|(dropped, _)| dropped == *candidate)
        })
        .cloned()
        .collect();
    report.remaining.clone()
}

/// Whether a definition is still in play (ADR-327 item 4): every definition in a plan whose
/// selection does not narrow; in one that does, a definition an entry of a remaining candidate
/// lists. Callers: `next_definition`, `mark_unread`.
fn in_play(plan: &CheckedPlan, remaining: &[String], definition: &str) -> bool {
    !plan.selection.narrows() || named_by(plan, remaining, definition) > 0
}

/// How many of the `remaining` candidates list `definition` in an entry.
fn named_by(plan: &CheckedPlan, remaining: &[String], definition: &str) -> usize {
    remaining
        .iter()
        .filter(|candidate| {
            plan.entries.iter().any(|(owner, entry)| {
                owner == *candidate && entry.alternatives.iter().any(|a| a == definition)
            })
        })
        .count()
}

/// The distinct candidate sets left by combining, per endpoint, one of its alternatives: each
/// alternative is the set of candidates it drops. Starts from `remaining` and, for each endpoint,
/// replaces every set by itself minus each alternative's drops, deduplicated after each endpoint,
/// so the collection never exceeds the distinct subsets reachable (not the product of the
/// alternatives). Empty when there is no endpoint. Caller: `outcome_sets`.
fn fold_outcomes(remaining: &[String], endpoints: &[Vec<Vec<String>>]) -> Vec<Vec<String>> {
    if endpoints.is_empty() {
        return Vec::new();
    }
    let mut sets: Vec<Vec<String>> = vec![remaining.to_vec()];
    for alternatives in endpoints {
        let mut next: Vec<Vec<String>> = Vec::new();
        for set in &sets {
            for dropped in alternatives {
                let left: Vec<String> = set
                    .iter()
                    .filter(|candidate| !dropped.contains(candidate))
                    .cloned()
                    .collect();
                if !next.contains(&left) {
                    next.push(left);
                }
            }
        }
        sets = next;
    }
    sets
}

/// The distinct candidate sets the conclusive outcomes of reading `definition` would leave over
/// `remaining` (ADR-327 item 4). Per endpoint of the definition (its planned reads with an
/// endpoint and a request) the alternatives are identified there, and ruled out there where the
/// read can be conclusive (no variant of the definition has a refused link, ADR-401); each
/// alternative's drops are judged by `contradictions` over the facts so far and that one verdict
/// (a contradiction at an endpoint depends only on the facts at it, and the VIN rules do not
/// depend on the read), and `fold_outcomes` combines the endpoints without building their
/// product. No answer and an inconclusive read are not outcomes. A decoded rule field plugs in as
/// a further alternative of an endpoint (or of an endpoint and component) with its own drops.
/// Empty for a definition with no endpoint to read. A plan whose selection does not narrow leaves
/// every candidate in every outcome. Caller: `next_definition`.
fn outcome_sets(
    plan: &CheckedPlan,
    facts: &[Fact],
    vin_rules: &[VinRuleResult],
    rule_fields: &[RuleFieldRead],
    remaining: &[String],
    definition: &str,
) -> Vec<Vec<String>> {
    let mut endpoints: Vec<&EndpointKey> = Vec::new();
    for read in plan
        .ecu_reads
        .iter()
        .filter(|read| read.definition == definition && has_request(read))
    {
        if let Some(endpoint) = &read.endpoint
            && !endpoints.contains(&endpoint)
        {
            endpoints.push(endpoint);
        }
    }
    let mut verdicts = vec![Verdict::Identified];
    if !has_refused_variant(plan, definition) {
        verdicts.push(Verdict::RuledOut);
    }
    let mut alternatives: Vec<Vec<Vec<String>>> = endpoints
        .into_iter()
        .map(|endpoint| {
            verdicts
                .iter()
                .map(|&verdict| {
                    if !plan.selection.narrows() {
                        return Vec::new();
                    }
                    let mut all = facts.to_vec();
                    all.push(Fact {
                        definition: definition.to_owned(),
                        endpoint: endpoint.clone(),
                        verdict,
                    });
                    contradictions(plan, &all, vin_rules, rule_fields)
                        .into_iter()
                        .map(|(dropped, _)| dropped)
                        .collect()
                })
                .collect()
        })
        .collect();
    // A rule field is a further list of alternatives: unevaluated is not an outcome, as an
    // inconclusive read is not (ADR-403).
    for (k, field) in plan.rule_fields.iter().enumerate() {
        let read = &plan.ecu_reads[field.read];
        if read.definition == definition && read.endpoint.is_some() {
            alternatives.push(rule_classes(
                plan,
                facts,
                vin_rules,
                rule_fields,
                k,
                remaining,
            ));
        }
    }
    fold_outcomes(remaining, &alternatives)
}

/// The alternatives of rule field `k` for the outcomes of reading its definition (ADR-327 item 4,
/// ADR-403): per distinct value the `remaining` candidates' rules on it expect (built from
/// `expected` by the field's kind, so one expected value that is a prefix of another lands in one
/// class with it) the candidates dropped when the field is established to it, every rule on the
/// field judged by `expected_matches`; and the value no rule matches. Each class is judged by
/// `contradictions`, as the run is. A plan whose selection does not narrow drops nothing. Caller:
/// `outcome_sets`.
fn rule_classes(
    plan: &CheckedPlan,
    facts: &[Fact],
    vin_rules: &[VinRuleResult],
    rule_fields: &[RuleFieldRead],
    k: usize,
    remaining: &[String],
) -> Vec<Vec<String>> {
    if !plan.selection.narrows() {
        return vec![Vec::new()];
    }
    let kind = plan.rule_fields[k].kind;
    let mut values: Vec<PhysicalValue> = Vec::new();
    for rule in &rule_fields[k].rules {
        if let Some(value) = expected_value(kind, &rule.expected)
            && remaining.contains(&rule.candidate)
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    // The value no rule matches: a float, which no rule compares equal (ADR-306 item 5).
    let none = PhysicalValue::Float(0.0);
    let dropped_when = |value: &PhysicalValue| {
        let mut hypothetical = rule_fields.to_vec();
        let field = &mut hypothetical[k];
        field.established = Some(FieldValue::from(value));
        judge_rules(field, value);
        contradictions(plan, facts, vin_rules, &hypothetical)
            .into_iter()
            .map(|(dropped, _)| dropped)
            .collect::<Vec<String>>()
    };
    values
        .iter()
        .map(dropped_when)
        .chain([dropped_when(&none)])
        .collect()
}

/// The definition to read next, with its worst case (ADR-327 item 4): among those in play, not
/// yet tried and with a group that opens a link, the one with the smallest worst case (the
/// largest outcome set; every remaining candidate for a definition with no outcome), then named
/// by the most remaining candidates, then with the most endpoints that answered presence, then
/// the lowest identifier (byte order). `None` when no definition is left. Caller: `ecu_phase`.
fn next_definition(
    plan: &CheckedPlan,
    report: &IdentificationReport,
    remaining: &[String],
    tried: &HashSet<String>,
) -> Option<(String, usize)> {
    let facts = facts_of(&report.ecu_reads);
    let mut seen: Vec<&str> = Vec::new();
    let mut best: Option<(usize, Reverse<usize>, Reverse<usize>, &str)> = None;
    for read in &plan.ecu_reads {
        let definition = read.definition.as_str();
        if seen.contains(&definition) {
            continue;
        }
        seen.push(definition);
        let opens = plan
            .ecu_reads
            .iter()
            .any(|r| r.definition == definition && r.opens());
        if tried.contains(definition) || !opens || !in_play(plan, remaining, definition) {
            continue;
        }
        let worst = outcome_sets(
            plan,
            &facts,
            &report.vin_rules,
            &report.rule_fields,
            remaining,
            definition,
        )
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(remaining.len());
        let mut heard: Vec<&EndpointKey> = Vec::new();
        for endpoint in plan
            .ecu_reads
            .iter()
            .filter(|r| r.definition == definition && r.opens())
            .filter_map(|r| r.endpoint.as_ref())
        {
            if report.answered(endpoint) && !heard.contains(&endpoint) {
                heard.push(endpoint);
            }
        }
        let key = (
            worst,
            Reverse(named_by(plan, remaining, definition)),
            Reverse(heard.len()),
            definition,
        );
        if best.as_ref().is_none_or(|known| key < *known) {
            best = Some(key);
        }
    }
    best.map(|(worst, .., definition)| (definition.to_owned(), worst))
}

/// Marks the declarations of the definitions that were not read as `NotInPlay` when no remaining
/// candidate names the definition; those still in play stay `NotRead` (the run's bounds ran out).
/// Callers: both ends of `ecu_phase`.
fn mark_unread(plan: &CheckedPlan, report: &mut IdentificationReport, remaining: &[String]) {
    for read in &mut report.ecu_reads {
        if in_play(plan, remaining, &read.definition) {
            continue;
        }
        for request in &mut read.requests {
            for declaration in &mut request.declarations {
                if declaration.outcome == RequestOutcome::NotRead {
                    declaration.outcome = RequestOutcome::NotInPlay;
                }
            }
        }
    }
    for (field, planned) in report.rule_fields.iter_mut().zip(&plan.rule_fields) {
        if in_play(plan, remaining, &plan.ecu_reads[planned.read].definition) {
            continue;
        }
        for source in &mut field.sources {
            if source.outcome == RuleSourceOutcome::NotRead {
                source.outcome = RuleSourceOutcome::NotInPlay;
            }
        }
    }
}

/// The ECU phase: reads the definitions in the order of `next_definition`, each once per endpoint
/// and one link open at a time, narrowing the candidates of a plan whose selection narrows
/// before each choice, and sets the report's ECU reads (ADR-401, ADR-402). It ends when no
/// definition in play is left untried (which includes the end of every candidate in such a plan).
/// A run whose bounds run out in a read leaves the rest `NotRead` and keeps the drops made. Every
/// link is closed when this returns.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn ecu_phase<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    plan: &CheckedPlan,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    comm_log: &Option<crate::comm_log::CommLog>,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
    report: &mut IdentificationReport,
) -> Result<(), JobError> {
    let mut remaining = narrow(plan, report, None);
    let mut tried: HashSet<String> = HashSet::new();
    while let Some((definition, worst_case)) = next_definition(plan, report, &remaining, &tried) {
        report.read_order.push(ReadStep {
            definition: definition.clone(),
            worst_case,
        });
        for at in 0..plan.ecu_reads.len() {
            if plan.ecu_reads[at].definition != definition {
                continue;
            }
            let interrupted = read_definition_at(
                handle,
                source,
                plan,
                at,
                limits,
                cancelled,
                guards,
                comm_log,
                reopens,
                worker_ended,
                report,
            )?;
            if interrupted {
                report.ecu_reads_interrupted = true;
                // A match from the part read counts as a fact; no interrupted read is conclusive.
                remaining = narrow(plan, report, Some(&definition));
                mark_unread(plan, report, &remaining);
                return Ok(());
            }
        }
        remaining = narrow(plan, report, Some(&definition));
        tried.insert(definition);
    }
    mark_unread(plan, report, &remaining);
    Ok(())
}

/// Reads the planned read `plan.ecu_reads[at]`: each of its link groups that opens, then judges
/// the read (matches, answered, conclusive) and sets it in the report (ADR-401). The only caller
/// of `ecu_read_group`. Whether the run's bounds ran out in it.
#[expect(
    clippy::too_many_arguments,
    reason = "the run's whole context, split only by what the caller owns"
)]
fn read_definition_at<W: WorkerSource>(
    handle: &Handle,
    source: &Arc<Mutex<W>>,
    plan: &CheckedPlan,
    at: usize,
    limits: &JobLimits,
    cancelled: &AtomicBool,
    guards: &GuardSlot,
    comm_log: &Option<crate::comm_log::CommLog>,
    reopens: &mut u8,
    worker_ended: &Cell<bool>,
    report: &mut IdentificationReport,
) -> Result<bool, JobError> {
    let planned = &plan.ecu_reads[at];
    let mut decoded: HashMap<(Sha256, u32), Vec<DecodedField>> = HashMap::new();
    let mut first = 0;
    let mut interrupted = false;
    for group in &planned.groups {
        let count = group.requests.len();
        let Some(link) = group.link.as_ref().filter(|_| count > 0) else {
            first += count;
            continue;
        };
        let end = ecu_read_group(
            handle,
            source,
            group,
            link,
            &plan.documents,
            comm_log.clone(),
            limits,
            cancelled,
            guards,
            reopens,
            worker_ended,
        )?;
        let reported = &mut report.ecu_reads[at].requests[first..first + count];
        match end {
            None => {
                for request in reported {
                    for declaration in &mut request.declarations {
                        declaration.outcome = RequestOutcome::WorkerRefused;
                    }
                }
            }
            Some(read) => {
                for (request, outcomes) in reported.iter_mut().zip(read.outcomes) {
                    for (declaration, outcome) in request.declarations.iter_mut().zip(outcomes) {
                        declaration.outcome = outcome;
                    }
                }
                decoded.extend(read.decoded);
                interrupted = read.interrupted;
            }
        }
        first += count;
        if interrupted {
            break;
        }
    }
    let reported = &mut report.ecu_reads[at];
    // Every match is reported, not the first by name (ADR-327 item 7).
    for (hash, document) in &plan.documents {
        if reported.documents.contains(hash)
            && variant_matches(&document.variant(), |service| {
                decoded.get(&(*hash, service)).map(Vec::as_slice)
            })
            && !reported.matching.contains(hash)
        {
            reported.matching.push(*hash);
        }
    }
    // The answered judgement counts every declaration: an ECU that answered a rule's request is
    // there. Conclusiveness counts the pattern declarations only (ADR-327 item 3, ADR-403).
    let outcomes = || {
        reported
            .requests
            .iter()
            .flat_map(|request| &request.declarations)
            .map(|declaration| &declaration.outcome)
    };
    let pattern_outcomes = || {
        reported
            .requests
            .iter()
            .flat_map(|request| &request.declarations)
            .filter(|declaration| declaration.pattern)
            .map(|declaration| &declaration.outcome)
    };
    let answered = outcomes().any(|o| {
        matches!(
            o,
            RequestOutcome::Decoded(_) | RequestOutcome::Undecodable | RequestOutcome::Negative(_)
        )
    });
    // The one place that judges a read conclusive: no negative response is definitive until
    // the IR declares which are, and a positive response that does not decode rules nothing
    // out (ADR-327 item 3, ADR-401). A variant whose link is refused leaves the definition
    // inconclusive at every endpoint.
    let refused_variant = has_refused_variant(plan, &planned.definition);
    let conclusive = pattern_outcomes().next().is_some()
        && pattern_outcomes().all(|o| matches!(o, RequestOutcome::Decoded(_)))
        && !refused_variant;
    reported.answered = answered;
    reported.conclusive = conclusive;
    evaluate_rule_fields(plan, at, &decoded, report);
    Ok(interrupted)
}

/// Judges the rule fields of the planned read `plan.ecu_reads[at]` from what its declarations
/// gave and the fields of the groups just read (`decoded`): each source's outcome, the value the
/// component is established to when every source decoded and all agree (a text and bytes of equal
/// content differ: another layout decoding to another value), and each rule on the field,
/// matched or not by `expected_matches` as a pattern is (ADR-327 item 4, ADR-306, ADR-403). A rule
/// of a field that is not established stays unevaluated. Caller: `read_definition_at`.
fn evaluate_rule_fields(
    plan: &CheckedPlan,
    at: usize,
    decoded: &HashMap<(Sha256, u32), Vec<DecodedField>>,
    report: &mut IdentificationReport,
) {
    let declared = |document: Sha256, service: u32| {
        report.ecu_reads[at]
            .requests
            .iter()
            .flat_map(|request| &request.declarations)
            .find(|d| d.document == document && d.service == service)
            .map(|d| d.outcome.clone())
    };
    let mut judged = Vec::new();
    for (k, planned) in plan
        .rule_fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.read == at)
    {
        let mut values: Vec<Option<PhysicalValue>> = Vec::new();
        let mut sources = Vec::new();
        for source in &planned.sources {
            let value = decoded
                .get(&(source.document, source.service))
                .and_then(|fields| fields.iter().find(|f| f.id == source.field))
                .map(|f| f.value.clone());
            let outcome = match declared(source.document, source.service) {
                Some(RequestOutcome::Decoded(_)) => match &value {
                    Some(value) => RuleSourceOutcome::Decoded(FieldValue::from(value)),
                    None => RuleSourceOutcome::Undecodable,
                },
                Some(RequestOutcome::Undecodable) | None => RuleSourceOutcome::Undecodable,
                Some(RequestOutcome::Negative(nrc)) => RuleSourceOutcome::Negative(nrc),
                Some(RequestOutcome::NoUsableAnswer) => RuleSourceOutcome::NoUsableAnswer,
                Some(RequestOutcome::LinkRefused) => RuleSourceOutcome::LinkRefused,
                Some(RequestOutcome::WorkerRefused) => RuleSourceOutcome::WorkerRefused,
                Some(RequestOutcome::NotRead) => RuleSourceOutcome::NotRead,
                Some(RequestOutcome::NotInPlay) => RuleSourceOutcome::NotInPlay,
            };
            values.push(match outcome {
                RuleSourceOutcome::Decoded(_) => value,
                _ => None,
            });
            sources.push(RuleFieldSource {
                document: source.document,
                service: source.service,
                field: source.field,
                outcome,
            });
        }
        let established = agreed(values);
        judged.push((k, sources, established));
    }
    for (k, sources, established) in judged {
        let field = &mut report.rule_fields[k];
        field.sources = sources;
        field.established = established.as_ref().map(FieldValue::from);
        match &established {
            Some(value) => judge_rules(field, value),
            None => {
                for rule in &mut field.rules {
                    rule.outcome = RuleOutcome::Unevaluated;
                }
            }
        }
    }
}

/// Sets each rule on `field` matched or not by `expected_matches` for the established `value`
/// (ADR-306, ADR-403). Callers: `evaluate_rule_fields`, `rule_classes`.
fn judge_rules(field: &mut RuleFieldRead, value: &PhysicalValue) {
    for rule in &mut field.rules {
        rule.outcome = if expected_matches(&rule.expected, value) {
            RuleOutcome::Matched
        } else {
            RuleOutcome::NotMatched
        };
    }
}

/// The distinct component values the matched rules of the remaining candidates give: the VIN
/// rules' first, then the field rules' (ADR-403). A dropped candidate's matched rule gives none,
/// though its sources still counted in establishing the field. Caller: `identify`, last.
fn component_values(report: &IdentificationReport) -> Vec<ComponentValue> {
    let mut values: Vec<ComponentValue> = Vec::new();
    let kept = |candidate: &String| report.remaining.contains(candidate);
    let matched = report
        .vin_rules
        .iter()
        .filter(|rule| rule.outcome == RuleOutcome::Matched && kept(&rule.candidate))
        .map(|rule| (rule.component, &rule.value))
        .chain(
            report
                .rule_fields
                .iter()
                .flat_map(|field| field.rules.iter().map(|rule| (field.component, rule)))
                .filter(|(_, rule)| rule.outcome == RuleOutcome::Matched && kept(&rule.candidate))
                .map(|(component, rule)| (component, &rule.value)),
        );
    for (component, value) in matched {
        if !values
            .iter()
            .any(|known| known.component == component && known.value == *value)
        {
            values.push(ComponentValue {
                component,
                value: value.clone(),
            });
        }
    }
    values
}

/// Sets each entry's state from the reads of its alternatives (ADR-326 item 3, ADR-327 item 3,
/// ADR-401): the only writer of the entry states, and called only for a phase that ran to its end
/// (an interrupted one leaves every entry not read). An entry with an alternative that had no
/// physical read (every declaration not read or refused by the worker) is not read, whatever the
/// others show; an alternative with no declaration at all (no pattern) is left out, and an entry
/// with none left is not read. Else one alternative with a matching variant identifies the
/// entry, two make it ambiguous; else an endpoint that answered is unrecognized when every read
/// there was conclusive and no alternative has a variant whose link is refused, and the entry is
/// answered but not identified otherwise; else the entry is not answering (not fitted when
/// optional) if a request was sent, and not read if none was.
fn evaluate_entries(entries: &mut [EntryResult], reads: &[EcuRead]) {
    for entry in entries {
        // A read with no pattern declaration (a definition read only for a field rule) is judged
        // as if the definition had no declaration (ADR-401 item 8, ADR-403).
        let relevant: Vec<&EcuRead> = reads
            .iter()
            .filter(|read| entry.alternatives.contains(&read.definition))
            .filter(|read| {
                read.requests
                    .iter()
                    .any(|request| request.declarations.iter().any(|d| d.pattern))
            })
            .collect();
        let outcomes = |definition: &String| {
            relevant
                .iter()
                .filter(|read| read.definition == *definition)
                .flat_map(|read| &read.requests)
                .flat_map(|request| &request.declarations)
                .filter(|d| d.pattern)
                .map(|d| &d.outcome)
                .collect::<Vec<_>>()
        };
        // An alternative with no declaration (no variant has a pattern) contributes nothing.
        let judged: Vec<Vec<&RequestOutcome>> = entry
            .alternatives
            .iter()
            .map(outcomes)
            .filter(|outcomes| !outcomes.is_empty())
            .collect();
        let physically_read = |outcomes: &Vec<&RequestOutcome>| {
            outcomes.iter().any(|o| {
                !matches!(
                    o,
                    RequestOutcome::NotRead
                        | RequestOutcome::NotInPlay
                        | RequestOutcome::WorkerRefused
                )
            })
        };
        if judged.is_empty() || !judged.iter().all(physically_read) {
            entry.state = EntryState::NotRead;
            continue;
        }
        // A variant whose link ADR-324 refuses leaves its definition, and so the entry,
        // inconclusive (a pattern-less one sends nothing and counts for nothing).
        let refused_variant = relevant
            .iter()
            .any(|read| read.endpoint.is_none() && !read.requests.is_empty());
        let matched: Vec<&String> = entry
            .alternatives
            .iter()
            .filter(|definition| {
                relevant
                    .iter()
                    .any(|read| read.definition == **definition && !read.matching.is_empty())
            })
            .collect();
        entry.state = match matched[..] {
            [definition] => EntryState::Identified {
                definition: definition.clone(),
                variants: relevant
                    .iter()
                    .filter(|read| read.definition == *definition)
                    .flat_map(|read| read.matching.iter().copied())
                    .collect(),
            },
            [_, ..] => EntryState::Ambiguous {
                definitions: matched.into_iter().cloned().collect(),
            },
            [] => {
                let mut answered: Vec<&EndpointKey> = Vec::new();
                for read in &relevant {
                    if let Some(endpoint) = &read.endpoint
                        && read.answered
                        && !answered.contains(&endpoint)
                    {
                        answered.push(endpoint);
                    }
                }
                // A read with no request (a pattern-less definition) is never conclusive and
                // counts for nothing here either.
                let settled = |endpoint: &&EndpointKey| {
                    relevant
                        .iter()
                        .filter(|read| {
                            read.endpoint.as_ref() == Some(*endpoint) && !read.requests.is_empty()
                        })
                        .all(|read| read.conclusive)
                        && !refused_variant
                };
                let sent = relevant
                    .iter()
                    .flat_map(|read| &read.requests)
                    .flat_map(|request| &request.declarations)
                    .filter(|d| d.pattern)
                    .any(|d| {
                        !matches!(
                            d.outcome,
                            RequestOutcome::LinkRefused
                                | RequestOutcome::WorkerRefused
                                | RequestOutcome::NotRead
                                | RequestOutcome::NotInPlay
                        )
                    });
                if !answered.is_empty() && answered.iter().all(settled) {
                    EntryState::UnrecognizedVariant {
                        endpoints: answered.into_iter().cloned().collect(),
                    }
                } else if !answered.is_empty() {
                    EntryState::AnsweredNotIdentified
                } else if sent && entry.optional {
                    EntryState::NotFitted
                } else if sent {
                    EntryState::NotAnswering
                } else {
                    EntryState::NotRead
                }
            }
        };
    }
}

/// Whether a link that did not open failed on a lost path, with nothing left open: the open is
/// tried again within the run's bounds (ADR-393). A refusal the worker gave is not a loss.
fn is_path_loss(refused: &LinkRefused) -> bool {
    refused.cleaned_up
        && match &refused.error {
            JobError::Link(error) => {
                !refused.table_refused
                    && (error.resend_class() == ResendClass::PathLost
                        || matches!(error, HostError::Rpc { status, .. } if names_lost_vci(status))
                        || matches!(
                            error,
                            HostError::WorkerStart {
                                spawn_failed: false,
                                ..
                            }
                        ))
            }
            _ => false,
        }
}

#[cfg(test)]
mod run_tests;

#[cfg(test)]
mod tests {
    use diag_ir::IR_SCHEMA_VERSION;
    use diag_ir::declaration::{
        ComParamT, CompuLinearT, CompuT, DataKind, EcuDocumentT, EcuVariantT, FieldT, Placement,
        ProtocolT, ServiceT, VariantPatternT, encode_ecu_document,
    };
    use diag_ir::plan::{
        Candidate, Component, Entry, FieldRule, PLAN_SCHEMA_VERSION, PlanDefinition,
        Selection as Chosen, Source, VinRule,
    };

    use super::*;
    use crate::document::tests::read_service;

    /// A document of one variant: service 1 reads a DID (request SID 0x22) with field 1, a
    /// pattern names it, and the protocol is UDS on CAN as `setup` leaves it. `did` tells
    /// documents apart.
    pub(super) fn document(
        did: u8,
        tx: u32,
        rx: u32,
        setup: impl FnOnce(&mut ProtocolT),
    ) -> Vec<u8> {
        document_reading(did, 4, tx, rx, setup)
    }

    /// [`document`] whose service 1 reads a field of `bytes` bytes; 17 makes the field a VIN
    /// source.
    pub(super) fn document_reading(
        did: u8,
        bytes: u32,
        tx: u32,
        rx: u32,
        setup: impl FnOnce(&mut ProtocolT),
    ) -> Vec<u8> {
        let mut protocol = ProtocolT::default();
        protocol.short_name = Some("ISO15765".into());
        protocol.application_layer = Some("UDS".into());
        protocol.baud_rate = 500_000;
        protocol.tx_id = tx;
        protocol.rx_id = rx;
        setup(&mut protocol);
        let mut pattern = VariantPatternT::default();
        pattern.service_id = 1;
        pattern.field_id = 1;
        pattern.expected = Some(vec![1]);
        let mut variant = EcuVariantT::default();
        variant.id = 1;
        variant.short_name = Some("v".into());
        variant.protocols = Some(vec![protocol]);
        variant.patterns = Some(vec![pattern]);
        variant.services = Some(vec![read_service(
            1,
            "read",
            [0xF1, did],
            DataKind::Ascii,
            bytes,
        )]);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        encode_ecu_document(&document)
    }

    /// `bytes` with its variant changed by `edit`.
    pub(super) fn edited(bytes: &[u8], edit: impl FnOnce(&mut EcuVariantT)) -> Vec<u8> {
        let mut variant = diag_ir::declaration::load_ecu_document(bytes)
            .unwrap()
            .variant()
            .unwrap()
            .unpack();
        edit(&mut variant);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        encode_ecu_document(&document)
    }

    /// Makes service 1's request a WriteDataByIdentifier (0x2E).
    fn write_service(variant: &mut EcuVariantT) {
        let service: &mut ServiceT = &mut variant.services.as_mut().unwrap()[0];
        service.request.as_mut().unwrap().sid = 0x2E;
    }

    pub(super) fn named(bytes: Vec<u8>) -> (Sha256, Vec<u8>) {
        (sha2::Sha256::digest(&bytes).into(), bytes)
    }

    fn with_param(name: &str, value: i64) -> impl FnOnce(&mut ProtocolT) {
        let mut param = ComParamT::default();
        param.short_name = Some(name.into());
        param.value = value;
        move |protocol| protocol.params.get_or_insert_default().push(param)
    }

    fn plan(documents: &[Sha256]) -> IdentificationPlan {
        let source = Source {
            document: documents[0],
            service: 1,
            field: 1,
        };
        IdentificationPlan {
            schema_version: PLAN_SCHEMA_VERSION,
            selection: Chosen::Candidates,
            candidates: vec![Candidate {
                id: "c1".into(),
                values: Vec::new(),
                vin_rules: Vec::new(),
                field_rules: vec![FieldRule {
                    component: Component::Oem,
                    value: "oem".into(),
                    source,
                    expected: vec![1],
                }],
                entries: vec![Entry {
                    role: "engine".into(),
                    optional: false,
                    gateway: None,
                    alternatives: vec!["ecu".into()],
                }],
            }],
            definitions: vec![PlanDefinition {
                id: "ecu".into(),
                documents: documents.to_vec(),
            }],
        }
    }

    fn load(
        plan: &IdentificationPlan,
        supplied: &[(Sha256, Vec<u8>)],
    ) -> Result<LoadedPlan, PlanRefused> {
        load_plan(&plan.encode().unwrap(), supplied.to_vec())
    }

    /// A plan over `documents` with one candidate whose VIN rules are `rules`: the index of the
    /// document whose service 1 field 1 is the source, then the positions and the expected text.
    pub(super) fn vin_plan(
        documents: &[Sha256],
        selection: Chosen,
        rules: &[(usize, u8, u8, &str)],
    ) -> IdentificationPlan {
        let mut plan = plan(documents);
        plan.selection = selection;
        plan.candidates[0].field_rules.clear();
        plan.candidates[0].vin_rules = rules
            .iter()
            .map(|&(at, start, end, expected)| VinRule {
                component: Component::ModelYear,
                value: "2020".into(),
                start,
                end,
                expected: expected.into(),
                source: Source {
                    document: documents[at],
                    service: 1,
                    field: 1,
                },
            })
            .collect();
        plan
    }

    /// Loads and checks [`vin_plan`] over `documents`, without the ECU reads: the presence and
    /// VIN tests isolate their phases from the ECU phase (the fixtures' documents carry
    /// identification patterns), which [`checked_vin_reading`] keeps.
    pub(super) fn checked_vin(
        documents: &[(Sha256, Vec<u8>)],
        selection: Chosen,
        rules: &[(usize, u8, u8, &str)],
    ) -> Result<CheckedPlan, PlanRefused> {
        let mut checked = checked_vin_reading(documents, selection, rules)?;
        checked.ecu_reads.clear();
        checked.rule_fields.clear();
        Ok(checked)
    }

    /// [`checked_vin`] with the ECU reads the plan derives.
    pub(super) fn checked_vin_reading(
        documents: &[(Sha256, Vec<u8>)],
        selection: Chosen,
        rules: &[(usize, u8, u8, &str)],
    ) -> Result<CheckedPlan, PlanRefused> {
        let hashes: Vec<Sha256> = documents.iter().map(|(hash, _)| *hash).collect();
        check_plan(&load(&vin_plan(&hashes, selection, rules), documents)?)
    }

    /// Loads and checks a plan over `documents`, without the ECU reads (see [`checked_vin`]).
    pub(super) fn checked(documents: &[(Sha256, Vec<u8>)]) -> Result<CheckedPlan, PlanRefused> {
        let mut checked = checked_reading(documents)?;
        checked.ecu_reads.clear();
        checked.rule_fields.clear();
        Ok(checked)
    }

    /// [`checked`] with the ECU reads the plan derives.
    pub(super) fn checked_reading(
        documents: &[(Sha256, Vec<u8>)],
    ) -> Result<CheckedPlan, PlanRefused> {
        let hashes: Vec<Sha256> = documents.iter().map(|(hash, _)| *hash).collect();
        check_plan(&load(&plan(&hashes), documents)?)
    }

    fn bus(baud_rate: u32) -> BusKey {
        BusKey {
            protocol: "ISO_14229_3_on_ISO_15765_2".into(),
            bus_type: "ISO_11898_2_DWCAN".into(),
            baud_rate,
        }
    }

    #[test]
    fn a_plan_of_an_unknown_schema_version_is_refused() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut other = plan(&[doc.0]);
        other.schema_version += 1;
        assert!(matches!(
            load(&other, &[doc]),
            Err(PlanRefused::Plan(PlanError::SchemaVersion {
                found: 2,
                expected: 1
            }))
        ));
    }

    #[test]
    fn a_missing_document_or_one_whose_hash_differs_is_refused_before_any_check() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let plan = plan(&[doc.0]);
        assert!(matches!(
            load(&plan, &[]),
            Err(PlanRefused::MissingDocument(missing)) if missing == hex(&doc.0)
        ));
        let other = document(0x91, 0x7E0, 0x7E8, |_| {});
        let actual = hex(&sha2::Sha256::digest(&other));
        assert!(matches!(
            load(&plan, &[(doc.0, other)]),
            Err(PlanRefused::HashMismatch { named, actual: found })
                if named == hex(&doc.0) && found == actual
        ));
        let garbage = named(b"not a document".to_vec());
        let plan = self::plan(&[garbage.0]);
        assert!(matches!(
            load(&plan, &[garbage]),
            Err(PlanRefused::BadDocument { .. })
        ));
    }

    #[test]
    fn a_service_outside_the_read_only_set_is_refused() {
        // Service 1 is a WriteDataByIdentifier (0x2E), named by the pattern and by the rule.
        let base = document(0x90, 0x7E0, 0x7E8, |_| {});
        let write = named(edited(&base, write_service));
        assert!(matches!(
            checked(std::slice::from_ref(&write)),
            Err(PlanRefused::NotReadOnly { document, service: 1 }) if document == hex(&write.0)
        ));
        // Through the rule's source alone.
        let only_rule = named(edited(&write.1, |v| v.patterns = None));
        assert!(matches!(
            checked(&[only_rule]),
            Err(PlanRefused::NotReadOnly { service: 1, .. })
        ));
        // Through a pattern alone, in a second document no source names.
        let ok = named(base);
        let pattern_only = named(edited(&document(0x91, 0x7E1, 0x7E9, |_| {}), write_service));
        assert!(matches!(
            checked(&[ok, pattern_only.clone()]),
            Err(PlanRefused::NotReadOnly { document, service: 1 }) if document == hex(&pattern_only.0)
        ));
    }

    #[test]
    fn a_variant_without_a_protocol_names_no_read_only_service() {
        // `ApplicationLayer::of_job` gives it no list, so its pattern is refused as a service
        // outside the read-only set, not reported as a refused link (ADR-395 item 3).
        let bare = named(edited(&document(0x90, 0x7E0, 0x7E8, |_| {}), |v| {
            v.protocols = None
        }));
        assert!(matches!(
            checked(&[bare]),
            Err(PlanRefused::NotReadOnly { service: 1, .. })
        ));
    }

    #[test]
    fn a_source_naming_a_field_the_response_lacks_is_refused() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut bad = plan(&[doc.0]);
        bad.candidates[0].field_rules[0].source.field = 9;
        assert!(matches!(
            check_plan(&load(&bad, &[doc]).unwrap()),
            Err(PlanRefused::UnknownField {
                service: 1,
                field: 9,
                ..
            })
        ));
    }

    #[test]
    fn a_link_adr_324_refuses_is_reported_and_leaves_the_plan_valid() {
        let ok = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let refused = named(document(0x91, 0x7E1, 0x7E9, |p| {
            p.extended_addressing = true
        }));
        let report = checked(&[ok.clone(), refused.clone()]).unwrap().report();
        assert_eq!(report.refused_links().len(), 1);
        assert_eq!(report.refused_links()[0].document, refused.0);
        assert!(
            report.refused_links()[0]
                .reason
                .contains("extended addressing")
        );
        // The refused document gives no endpoint.
        assert_eq!(report.endpoints().len(), 1);
        assert_eq!(report.endpoints()[0].documents, vec![ok.0]);
    }

    #[test]
    fn links_of_one_presence_group_that_differ_in_a_comparam_other_than_timing_are_refused() {
        let grouped = |param: &'static str, value: i64| {
            move |p: &mut ProtocolT| {
                p.functional_id = 0x7DF;
                with_param(param, value)(p);
            }
        };
        let first = named(document(0x90, 0x7E0, 0x7E8, grouped("CP_BlockSize", 8)));
        let second = named(document(0x91, 0x7E1, 0x7E9, grouped("CP_BlockSize", 16)));
        assert!(matches!(
            checked(&[first.clone(), second.clone()]),
            Err(PlanRefused::ComParamsDiffer { first: a, second: b })
                if a == hex(&first.0) && b == hex(&second.0)
        ));
        // A ComParam declared on one link and missing on the other differs.
        let bare = named(document(0x92, 0x7E2, 0x7EA, |p| p.functional_id = 0x7DF));
        assert!(matches!(
            checked(&[first.clone(), bare]),
            Err(PlanRefused::ComParamsDiffer { .. })
        ));
        // Names are compared ignoring case, as the workers resolve them (ADR-324 item 3).
        let lower = named(document(0x93, 0x7E3, 0x7EB, grouped("cp_blocksize", 8)));
        assert!(checked(&[first.clone(), lower]).is_ok());
        let lower = named(document(0x93, 0x7E3, 0x7EB, grouped("cp_blocksize", 9)));
        assert!(matches!(
            checked(&[first.clone(), lower]),
            Err(PlanRefused::ComParamsDiffer { .. })
        ));
        // Other presence groups of the bus, and links without a group, are not compared.
        let other_group = named(document(0x94, 0x7E4, 0x7EC, |p| {
            p.functional_id = 0x7DE;
            with_param("CP_BlockSize", 16)(p);
        }));
        let no_group = named(document(0x95, 0x7E5, 0x7ED, with_param("CP_BlockSize", 32)));
        assert!(checked(&[first, other_group, no_group]).is_ok());
    }

    #[test]
    fn differing_listed_timings_are_not_refused_and_the_largest_of_each_is_taken() {
        let timed = |values: [(&'static str, i64); 4]| {
            move |p: &mut ProtocolT| {
                p.functional_id = 0x7DF;
                for (name, value) in values {
                    with_param(name, value)(p);
                }
            }
        };
        let first = named(document(
            0x90,
            0x7E0,
            0x7E8,
            timed([
                ("CP_P2Max", 50_000),
                ("CP_P2Star", 3_000_000),
                ("CP_Cr", 1_000_000),
                ("cp_p3func", 5_000),
            ]),
        ));
        let second = named(document(
            0x91,
            0x7E1,
            0x7E9,
            timed([
                ("CP_P2Max", 60_000),
                ("CP_RC78CompletionTimeout", 30_000_000),
                ("CP_Cr", 2_000_000),
                ("CP_P3Func", 4_000),
            ]),
        ));
        let group = &checked(&[first, second])
            .unwrap()
            .report()
            .presence_groups()[0]
            .clone();
        assert_eq!(
            (group.p2_max_ms, group.p2_star_ms, group.rc78_completion_ms),
            (60, 5_000, 30_000)
        );
        assert_eq!(
            group.timings,
            [
                ("cp_cr".to_owned(), 2_000_000),
                ("cp_p3func".to_owned(), 5_000)
            ]
        );
    }

    #[test]
    fn one_resource_at_two_baud_rates_is_refused_before_any_link_opens() {
        let first = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let second = named(document(0x91, 0x7E1, 0x7E9, |p| p.baud_rate = 250_000));
        assert!(matches!(
            checked(&[first.clone(), second.clone()]),
            Err(PlanRefused::BaudRatesDiffer { first: a, second: b })
                if a == hex(&first.0) && b == hex(&second.0)
        ));
    }

    #[test]
    fn the_presence_p2_is_the_longest_of_the_groups_links_whatever_their_order() {
        let timing = |p2_us: i64, functional_id: u32| {
            move |p: &mut ProtocolT| {
                p.functional_id = functional_id;
                with_param("CP_P2Max", p2_us)(p);
            }
        };
        let group = |p2_max_ms| PresenceGroup {
            bus: bus(500_000),
            functional_id: 0x7DF,
            p2_max_ms,
            p2_star_ms: 5_000,
            rc78_completion_ms: 25_000,
            timings: Vec::new(),
        };
        let slow = named(document(0x91, 0x7E1, 0x7E9, timing(100_001, 0x7DF)));
        let fast = named(document(0x90, 0x7E0, 0x7E8, timing(50_000, 0x7DF)));
        // 100001 microseconds round up to 101 ms (ADR-324 item 3).
        let report = checked(&[slow.clone(), fast.clone()]).unwrap().report();
        assert_eq!(report.presence_groups(), [group(101)]);
        let report = checked(&[fast.clone(), slow]).unwrap().report();
        assert_eq!(report.presence_groups(), [group(101)]);
        // A link of another group or without one does not lengthen this group's window.
        let other = named(document(0x92, 0x7E2, 0x7EA, timing(200_000, 0x7DE)));
        let none = named(document(0x93, 0x7E3, 0x7EB, timing(300_000, 0)));
        let report = checked(&[fast, other, none]).unwrap().report();
        assert_eq!(report.presence_groups()[0], group(50));
        assert_eq!(report.presence_groups()[1].p2_max_ms, 200);
    }

    #[test]
    fn a_functional_id_is_as_wide_as_the_physical_ones_and_none_of_them() {
        let wide = named(document(0x90, 0x7E0, 0x7E8, |p| p.functional_id = 0x800));
        assert!(matches!(
            checked(&[wide]),
            Err(PlanRefused::BadFunctionalId {
                functional_id: 0x800,
                ..
            })
        ));
        // The same value as another link's response identifier on the bus.
        let first = named(document(0x90, 0x7E0, 0x7E8, |p| p.functional_id = 0x7E9));
        let second = named(document(0x91, 0x7E1, 0x7E9, |_| {}));
        assert!(matches!(
            checked(&[first, second.clone()]),
            Err(PlanRefused::BadFunctionalId {
                functional_id: 0x7E9,
                ..
            })
        ));
        // And as its own request identifier.
        let own = named(document(0x92, 0x7E0, 0x7E8, |p| p.functional_id = 0x7E0));
        assert!(matches!(
            checked(&[own]),
            Err(PlanRefused::BadFunctionalId {
                functional_id: 0x7E0,
                ..
            })
        ));
    }

    #[test]
    fn two_documents_on_one_endpoint_are_one_entry_counting_for_both() {
        let first = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let second = named(document(0x91, 0x7E0, 0x7E8, |p| {
            p.short_name = Some("ISO_14229_3_on_ISO_15765_2".into())
        }));
        let report = checked(&[first.clone(), second.clone()]).unwrap().report();
        assert_eq!(
            report.endpoints(),
            [EndpointEntry {
                key: EndpointKey {
                    bus: bus(500_000),
                    tx_id: 0x7E0,
                    rx_id: 0x7E8
                },
                documents: vec![first.0, second.0]
            }]
        );
    }

    #[test]
    fn a_bus_without_a_functional_id_has_no_presence_group_and_is_reported() {
        let first = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let second = named(document(0x91, 0x7E1, 0x7E9, |_| {}));
        let report = checked(&[first.clone(), second]).unwrap().report();
        assert!(report.presence_groups().is_empty());
        assert_eq!(report.buses_without_presence(), [bus(500_000)]);
        // One link with a functional identifier gives the bus a group.
        let third = named(document(0x92, 0x7E2, 0x7EA, |p| p.functional_id = 0x7DF));
        let report = checked(&[first, third]).unwrap().report();
        assert_eq!(report.presence_groups().len(), 1);
        assert!(report.buses_without_presence().is_empty());
    }

    #[test]
    fn duplicate_candidate_ids_and_an_unknown_definition_are_refused() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut twice = plan(&[doc.0]);
        twice.candidates.push(twice.candidates[0].clone());
        assert!(matches!(
            load(&twice, std::slice::from_ref(&doc)),
            Err(PlanRefused::Plan(PlanError::DuplicateCandidate(id))) if id == "c1"
        ));
        let mut unknown = plan(&[doc.0]);
        unknown.candidates[0].entries[0].alternatives = vec!["gone".into()];
        assert!(matches!(
            load(&unknown, &[doc]),
            Err(PlanRefused::Plan(PlanError::UnknownDefinition { definition, .. }))
                if definition == "gone"
        ));
    }

    #[test]
    fn a_valid_plan_gives_endpoints_presence_groups_and_an_empty_presence_section() {
        let first = named(document(0x90, 0x7E0, 0x7E8, |p| p.functional_id = 0x7DF));
        let second = named(document(0x91, 0x7E1, 0x7E9, |p| p.functional_id = 0x7DF));
        let report = checked(&[first.clone(), second.clone()]).unwrap().report();
        assert_eq!(report.selection(), Chosen::Candidates);
        assert_eq!(report.candidates(), ["c1".to_owned()]);
        let keys: Vec<(u32, u32)> = report
            .endpoints()
            .iter()
            .map(|entry| (entry.key.tx_id, entry.key.rx_id))
            .collect();
        assert_eq!(keys, [(0x7E0, 0x7E8), (0x7E1, 0x7E9)]);
        assert_eq!(report.presence_groups().len(), 1);
        assert_eq!(report.presence_groups()[0].p2_max_ms, 1_000);
        assert!(report.buses_without_presence().is_empty());
        assert!(report.refused_links().is_empty());
        assert!(report.presence().is_empty());
    }

    /// The report is the CLI's JSON: hashes and heard bytes as lowercase hex, the selection by
    /// name, and the presence section with its bookkeeping.
    #[test]
    fn the_report_serializes_with_hashes_and_bytes_as_hex() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |p| p.functional_id = 0x7DF));
        let mut report = checked(std::slice::from_ref(&doc)).unwrap().report();
        report.presence.push(PresenceAnswer {
            bus: bus(500_000),
            rx_id: 0x7E8,
            bytes: vec![0x7E, 0x00],
        });
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["selection"], "Candidates");
        assert_eq!(json["endpoints"][0]["documents"][0], hex(&doc.0));
        assert_eq!(json["endpoints"][0]["key"]["tx_id"], 0x7E0);
        assert_eq!(json["presence"][0]["bytes"], "7e00");
        assert_eq!(json["presence"][0]["bus"]["baud_rate"], 500_000);
        assert_eq!(json["presence_links_opened"], 0);
        assert_eq!(json["refused_tables"], serde_json::json!([]));
        assert_eq!(json["presence_interrupted"], false);
    }

    #[test]
    fn the_report_carries_how_the_vehicle_was_chosen() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut chosen = plan(&[doc.0]);
        chosen.selection = Chosen::ChosenDefinition;
        let report = check_plan(&load(&chosen, std::slice::from_ref(&doc)).unwrap())
            .unwrap()
            .report();
        assert_eq!(report.selection(), Chosen::ChosenDefinition);
        let mut every = plan(&[doc.0]);
        every.selection = Chosen::EveryEcu;
        every.candidates.clear();
        let report = check_plan(&load(&every, &[doc]).unwrap()).unwrap().report();
        assert_eq!(report.selection(), Chosen::EveryEcu);
        assert!(report.candidates().is_empty());
        assert_eq!(report.endpoints().len(), 1);
    }

    fn vin_doc(did: u8, tx: u32, rx: u32) -> (Sha256, Vec<u8>) {
        named(document_reading(did, 17, tx, rx, |_| {}))
    }

    /// `doc` with its variant's name changed, so another document declares the same service.
    fn renamed(doc: &(Sha256, Vec<u8>)) -> (Sha256, Vec<u8>) {
        named(edited(&doc.1, |v| v.short_name = Some("other".into())))
    }

    const MODEL_YEAR: (usize, u8, u8, &str) = (0, 10, 10, "L");

    /// ADR-399 item 2: a VIN rule's source that is not 17 bytes of ASCII text in the ADR-345
    /// table's shape is refused before any link opens, with the document and the source.
    #[test]
    fn a_vin_source_that_is_not_17_ascii_bytes_is_refused() {
        // Too short.
        let short = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        assert!(matches!(
            checked_vin(std::slice::from_ref(&short), Chosen::Candidates, &[MODEL_YEAR]),
            Err(PlanRefused::VinSource { document, service: 1, field: 1, .. })
                if document == hex(&short.0)
        ));
        // Not text.
        let number = named(edited(&vin_doc(0x90, 0x7E0, 0x7E8).1, |v| {
            let field = &mut v.services.as_mut().unwrap()[0]
                .positive
                .as_mut()
                .unwrap()
                .fields
                .as_mut()
                .unwrap()[0];
            field.kind = DataKind::Uint;
            field.bit_length = 56;
        }));
        assert!(matches!(
            checked_vin(&[number], Chosen::Candidates, &[MODEL_YEAR]),
            Err(PlanRefused::VinSource { .. })
        ));
        // The same field as a field rule's source is not a VIN source, and is accepted.
        let mut as_field_rule = plan(&[short.0]);
        as_field_rule.candidates[0].vin_rules.clear();
        assert!(check_plan(&load(&as_field_rule, &[short]).unwrap()).is_ok());
    }

    /// The ADR-345 table reads a field that ends the response; a VIN field with another field
    /// after it cannot be a VIN source (ADR-399 consequences).
    #[test]
    fn a_vin_field_another_field_follows_is_refused() {
        let trailing = named(edited(&vin_doc(0x90, 0x7E0, 0x7E8).1, |v| {
            let mut extra = FieldT::default();
            extra.id = 2;
            extra.short_name = Some("status".into());
            extra.placement = Placement::AfterPrevious;
            extra.bit_length = 8;
            extra.kind = DataKind::Uint;
            v.services.as_mut().unwrap()[0]
                .positive
                .as_mut()
                .unwrap()
                .fields
                .as_mut()
                .unwrap()
                .push(extra);
        }));
        assert!(matches!(
            checked_vin(&[trailing], Chosen::Candidates, &[MODEL_YEAR]),
            Err(PlanRefused::VinSource { why, .. }) if why.contains("reaches past")
        ));
    }

    /// The table reads a field that follows the echo at once (offset 2): a VIN field with another
    /// field before it is refused too, though the declaration itself is well-formed.
    #[test]
    fn a_vin_field_another_field_precedes_is_refused() {
        let leading = named(edited(&vin_doc(0x90, 0x7E0, 0x7E8).1, |v| {
            let mut first = FieldT::default();
            first.id = 2;
            first.short_name = Some("status".into());
            first.placement = Placement::AfterPrevious;
            first.bit_length = 8;
            first.kind = DataKind::Uint;
            let fields = v.services.as_mut().unwrap()[0]
                .positive
                .as_mut()
                .unwrap()
                .fields
                .as_mut()
                .unwrap();
            fields.insert(0, first);
        }));
        assert!(matches!(
            checked_vin(&[leading], Chosen::Candidates, &[MODEL_YEAR]),
            Err(PlanRefused::VinSource { why, .. }) if why.contains("offset 2")
        ));
    }

    #[test]
    fn a_plan_without_a_vin_rule_has_no_vin_read() {
        let doc = vin_doc(0x90, 0x7E0, 0x7E8);
        let plan = checked_vin(&[doc], Chosen::Candidates, &[]).unwrap();
        assert!(plan.vin_reads.is_empty() && plan.vin_rules.is_empty());
        let report = plan.report();
        assert!(report.vin_reads().is_empty() && report.vin_rules().is_empty());
        assert!(report.vin().is_none() && !report.vin_interrupted());
    }

    /// ADR-399 item 1: sources of one endpoint with one request are one read, whatever document
    /// declares them, and its link takes the largest of their timings; a source named by two
    /// rules is one source.
    #[test]
    fn sources_of_one_endpoint_and_request_are_one_read_with_the_largest_timings() {
        let timed = |p2: i64, cr: i64| {
            named(document_reading(0x90, 17, 0x7E0, 0x7E8, move |p| {
                with_param("CP_P2Max", p2)(p);
                with_param("CP_Cr", cr)(p);
            }))
        };
        let (first, second) = (timed(50_000, 2_000_000), timed(60_000, 1_000_000));
        let plan = checked_vin(
            &[first, second],
            Chosen::Candidates,
            &[(0, 10, 10, "L"), (1, 11, 11, "M"), (0, 1, 3, "WVW")],
        )
        .unwrap();
        assert_eq!(plan.vin_reads.len(), 1);
        let read = &plan.vin_reads[0];
        assert_eq!(read.declarations.len(), 2);
        assert_eq!(read.request, [0x22, 0xF1, 0x90]);
        let link = read.link.as_ref().unwrap();
        assert_eq!(link.p2_max_ms, 60);
        assert_eq!(
            comparams(link),
            read.non_timing,
            "the timings are not among the ComParams compared"
        );
        let cr = link.params.iter().find(|(k, _)| k == "CP_Cr").unwrap();
        assert_eq!(cr.1, 2_000_000);
        let reported = plan.report();
        assert_eq!(reported.vin_reads().len(), 1);
        assert_eq!(reported.vin_rules().len(), 3);
        assert!(
            reported
                .vin_reads()
                .iter()
                .flat_map(|r| &r.sources)
                .all(|s| s.outcome == VinOutcome::NotRead)
        );
    }

    /// ADR-399 item 1: another request, another endpoint or another non-timing ComParam makes
    /// another read, and the reads come in the order their first source comes in the plan.
    #[test]
    fn reads_that_differ_in_request_endpoint_or_comparam_are_kept_apart_in_plan_order() {
        let base = vin_doc(0x90, 0x7E0, 0x7E8);
        let other_did = vin_doc(0x91, 0x7E0, 0x7E8);
        let other_endpoint = vin_doc(0x90, 0x7E1, 0x7E9);
        let other_param = named(document_reading(0x90, 17, 0x7E0, 0x7E8, |p| {
            with_param("CP_BlockSize", 16)(p)
        }));
        let same = renamed(&base);
        let plan = checked_vin(
            &[base, other_did, other_endpoint, other_param, same],
            Chosen::Candidates,
            &[
                (2, 1, 1, "W"),
                (0, 1, 1, "W"),
                (4, 2, 2, "V"),
                (1, 1, 1, "W"),
                (3, 1, 1, "W"),
            ],
        )
        .unwrap();
        let shape: Vec<_> = plan
            .vin_reads
            .iter()
            .map(|r| {
                (
                    r.endpoint.as_ref().unwrap().tx_id,
                    r.request[2],
                    r.declarations.len(),
                )
            })
            .collect();
        // The other endpoint first; then the base with the renamed copy; then the other DID;
        // the document with another ComParam last, on the base's endpoint and request.
        assert_eq!(
            shape,
            [
                (0x7E1, 0x90, 1),
                (0x7E0, 0x90, 2),
                (0x7E0, 0x91, 1),
                (0x7E0, 0x90, 1)
            ]
        );
    }

    /// ADR-399 item 4: a source whose document's link ADR-324 refuses is a read that is never
    /// made, reported as such; the plan stays valid and its other reads are kept.
    #[test]
    fn a_source_whose_link_is_refused_is_reported_as_such() {
        let ok = vin_doc(0x90, 0x7E0, 0x7E8);
        let refused = named(document_reading(0x90, 17, 0x7E1, 0x7E9, |p| {
            p.extended_addressing = true
        }));
        let plan = checked_vin(
            &[refused.clone(), ok],
            Chosen::Candidates,
            &[(0, 10, 10, "L"), (1, 10, 10, "L")],
        )
        .unwrap();
        let report = plan.report();
        let outcomes: Vec<_> = report
            .vin_reads()
            .iter()
            .map(|r| (r.endpoint.is_some(), r.sources[0].outcome.clone()))
            .collect();
        assert_eq!(
            outcomes,
            [
                (false, VinOutcome::LinkRefused),
                (true, VinOutcome::NotRead)
            ]
        );
        assert_eq!(report.vin_reads()[0].sources[0].document, refused.0);
        assert!(plan.vin_reads[0].link.is_none());
    }

    /// A candidate's id with its entries (role, optional, alternatives).
    pub(super) type CandidateEntries<'a> = (&'a str, &'a [(&'a str, bool, &'a [&'a str])]);

    /// A document on `endpoint` (with a functional identifier) whose variant `short_name` reads
    /// data identifier F1`did` as `bytes` bytes of text, and whose pattern expects the text to
    /// start with `expected`.
    pub(super) fn identified_by(
        did: u8,
        bytes: u32,
        expected: &[u8],
        short_name: &str,
        endpoint: (u32, u32),
        setup: impl FnOnce(&mut ProtocolT),
    ) -> (Sha256, Vec<u8>) {
        let base = document_reading(did, bytes, endpoint.0, endpoint.1, |protocol| {
            protocol.functional_id = 0x7DF;
            setup(protocol)
        });
        named(edited(&base, |variant| {
            variant.short_name = Some(short_name.into());
            variant.patterns.as_mut().unwrap()[0].expected = Some(expected.to_vec());
        }))
    }

    /// A plan over `documents` and its checked form: `definitions` names each definition with the
    /// indexes of its documents, `candidates` each candidate with its entries (role, optional,
    /// alternatives). A candidate-less plan is for every ECU.
    pub(super) fn ecu_plan(
        documents: &[(Sha256, Vec<u8>)],
        definitions: &[(&str, &[usize])],
        candidates: &[CandidateEntries<'_>],
    ) -> CheckedPlan {
        let selection = match candidates.len() {
            0 => Chosen::EveryEcu,
            1 => Chosen::ChosenDefinition,
            _ => Chosen::Candidates,
        };
        ecu_plan_of(selection, documents, definitions, candidates)
    }

    /// [`ecu_plan`] with the selection given.
    pub(super) fn ecu_plan_of(
        selection: Chosen,
        documents: &[(Sha256, Vec<u8>)],
        definitions: &[(&str, &[usize])],
        candidates: &[CandidateEntries<'_>],
    ) -> CheckedPlan {
        ecu_plan_ruled(selection, documents, definitions, candidates, &[])
    }

    /// A VIN rule of a model-year component: the candidate's id, the index of the document whose
    /// service 1 field 1 is the source, the positions compared and the expected text.
    pub(super) type CandidateVinRule<'a> = (&'a str, usize, u8, u8, &'a str);

    /// [`ecu_plan_of`] with VIN rules for the candidates.
    pub(super) fn ecu_plan_ruled(
        selection: Chosen,
        documents: &[(Sha256, Vec<u8>)],
        definitions: &[(&str, &[usize])],
        candidates: &[CandidateEntries<'_>],
        vin_rules: &[CandidateVinRule<'_>],
    ) -> CheckedPlan {
        ecu_plan_with_rules(
            selection,
            documents,
            definitions,
            candidates,
            vin_rules,
            &[],
        )
    }

    /// A field rule: the candidate's id, the index of the source's document, the source's service
    /// and field, the component, the value the rule gives and the expected bytes.
    pub(super) type CandidateFieldRule<'a> =
        (&'a str, usize, u32, u32, Component, &'a str, &'a [u8]);

    /// [`ecu_plan_ruled`] with field rules for the candidates.
    pub(super) fn ecu_plan_with_rules(
        selection: Chosen,
        documents: &[(Sha256, Vec<u8>)],
        definitions: &[(&str, &[usize])],
        candidates: &[CandidateEntries<'_>],
        vin_rules: &[CandidateVinRule<'_>],
        field_rules: &[CandidateFieldRule<'_>],
    ) -> CheckedPlan {
        try_ecu_plan_with_rules(
            selection,
            documents,
            definitions,
            candidates,
            vin_rules,
            field_rules,
        )
        .expect("a checked plan")
    }

    /// [`ecu_plan_with_rules`] that gives the refusal of a plan the agent's check refuses.
    pub(super) fn try_ecu_plan_with_rules(
        selection: Chosen,
        documents: &[(Sha256, Vec<u8>)],
        definitions: &[(&str, &[usize])],
        candidates: &[CandidateEntries<'_>],
        vin_rules: &[CandidateVinRule<'_>],
        field_rules: &[CandidateFieldRule<'_>],
    ) -> Result<CheckedPlan, PlanRefused> {
        let plan = IdentificationPlan {
            schema_version: PLAN_SCHEMA_VERSION,
            selection,
            candidates: candidates
                .iter()
                .map(|&(id, entries)| Candidate {
                    id: id.into(),
                    values: Vec::new(),
                    vin_rules: vin_rules
                        .iter()
                        .filter(|rule| rule.0 == id)
                        .map(|&(_, at, start, end, expected)| VinRule {
                            component: Component::ModelYear,
                            value: "2020".into(),
                            start,
                            end,
                            expected: expected.into(),
                            source: Source {
                                document: documents[at].0,
                                service: 1,
                                field: 1,
                            },
                        })
                        .collect(),
                    field_rules: field_rules
                        .iter()
                        .filter(|rule| rule.0 == id)
                        .map(
                            |&(_, at, service, field, component, value, expected)| FieldRule {
                                component,
                                value: value.into(),
                                source: Source {
                                    document: documents[at].0,
                                    service,
                                    field,
                                },
                                expected: expected.to_vec(),
                            },
                        )
                        .collect(),
                    entries: entries
                        .iter()
                        .map(|&(role, optional, alternatives)| Entry {
                            role: role.into(),
                            optional,
                            gateway: None,
                            alternatives: alternatives.iter().map(|a| (*a).into()).collect(),
                        })
                        .collect(),
                })
                .collect(),
            definitions: definitions
                .iter()
                .map(|&(id, at)| PlanDefinition {
                    id: id.into(),
                    documents: at.iter().map(|&i| documents[i].0).collect(),
                })
                .collect(),
        };
        check_plan(&load(&plan, documents)?)
    }

    /// `document` with `service` added to its variant's services.
    pub(super) fn with_service(
        document: &(Sha256, Vec<u8>),
        service: ServiceT,
    ) -> (Sha256, Vec<u8>) {
        named(edited(&document.1, |variant| {
            variant.services.as_mut().unwrap().push(service)
        }))
    }

    /// Service `id` reading F187 as two ASCII fields: field 1 of two bytes and field 2 of nine.
    pub(super) fn two_field_service(id: u32) -> ServiceT {
        let mut service = read_service(id, "readTwo", [0xF1, 0x87], DataKind::Ascii, 2);
        let fields = service.positive.as_mut().unwrap().fields.as_mut().unwrap();
        let mut second = fields[0].clone();
        second.id = 2;
        second.bit_length = 9 * 8;
        fields.push(second);
        service
    }

    /// The VIN section serializes to the report's JSON; a VIN is written only as the text of the
    /// established one, and `Debug` hides it.
    #[test]
    fn the_vin_section_serializes_and_debug_hides_the_vin() {
        let doc = vin_doc(0x90, 0x7E0, 0x7E8);
        let mut report = checked_vin(&[doc], Chosen::Candidates, &[MODEL_YEAR])
            .unwrap()
            .report();
        let vin = Vin::new("WVWZZZ1JZ3W386752".into());
        report.vin_reads[0].sources[0].outcome = VinOutcome::Decoded(vin.clone());
        report.vin_reads[0].accepted = Some(vin.clone());
        report.vin = Some(vin.clone());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["vin"], "WVWZZZ1JZ3W386752");
        assert_eq!(json["vin_reads"][0]["request"], "22f190");
        assert_eq!(
            json["vin_reads"][0]["sources"][0]["outcome"]["Decoded"],
            "WVWZZZ1JZ3W386752"
        );
        assert_eq!(json["vin_rules"][0]["outcome"], "Unevaluated");
        assert_eq!(json["vin_interrupted"], false);
        assert_eq!(json["vehicle_lock_held"], false);
        assert!(!format!("{report:?}").contains("WVWZZZ"));
    }

    // ------------------------------------------------------------ The ECU reads (ADR-401)

    /// A pattern service whose request needs a value cannot be built without one, which the
    /// check refuses before any link opens.
    #[test]
    fn an_identification_request_with_parameters_is_refused() {
        let base = document(0x90, 0x7E0, 0x7E8, |_| {});
        let scalar = named(edited(&base, |v| {
            let request = v.services.as_mut().unwrap()[0].request.as_mut().unwrap();
            let mut argument = FieldT::default();
            argument.id = 9;
            argument.short_name = Some("argument".into());
            argument.placement = Placement::AfterPrevious;
            argument.bit_length = 8;
            argument.kind = DataKind::Uint;
            request.params = Some(vec![argument]);
        }));
        assert!(matches!(
            checked_reading(std::slice::from_ref(&scalar)),
            Err(PlanRefused::IdentificationRequest { document, service: 1, .. })
                if document == hex(&scalar.0)
        ));
    }

    /// Variants of one definition are grouped by endpoint, and within an endpoint by the ComParams
    /// other than the timings, which a group raises to the largest; a request of equal bytes is one
    /// request of its group; reads come in plan order (definition, then first document).
    #[test]
    fn ecu_reads_group_variants_by_endpoint_and_comparams_in_plan_order() {
        let block = |size: i64, cr: i64| {
            move |p: &mut ProtocolT| {
                p.functional_id = 0;
                with_param("CP_BlockSize", size)(p);
                with_param("CP_Cr", cr)(p);
            }
        };
        let (a, b) = ((0x7E0, 0x7E8), (0x7E1, 0x7E9));
        let documents = [
            identified_by(0x87, 11, b"P", "v1", a, block(8, 1)),
            identified_by(0x87, 11, b"Q", "v2", a, block(16, 1)),
            identified_by(0x89, 5, b"R", "v3", a, block(8, 3)),
            identified_by(0x87, 11, b"S", "v4", b, block(8, 1)),
            identified_by(0x87, 11, b"T", "v5", a, block(8, 1)),
        ];
        let plan = ecu_plan(
            &documents,
            &[("D", &[0, 1, 2, 3]), ("E", &[4])],
            &[("c1", &[("engine", false, &["D", "E"])])],
        );
        let shape: Vec<_> = plan
            .ecu_reads
            .iter()
            .map(|read| {
                (
                    read.definition.as_str(),
                    read.endpoint.as_ref().map(|e| e.tx_id),
                    read.groups
                        .iter()
                        .map(|g| (g.documents.len(), g.requests.len()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("D", Some(0x7E0), vec![(2, 2), (1, 1)]),
                ("D", Some(0x7E1), vec![(1, 1)]),
                ("E", Some(0x7E0), vec![(1, 1)]),
            ]
        );
        let raised = plan.ecu_reads[0].groups[0].link.as_ref().unwrap();
        assert_eq!(
            raised.params.iter().find(|(k, _)| k == "CP_Cr").unwrap().1,
            3
        );
        // Equal request bytes are one request, whatever documents declare them.
        let twins = [
            identified_by(0x87, 11, b"P", "v1", a, block(8, 1)),
            identified_by(0x87, 11, b"Q", "v2", a, block(8, 1)),
        ];
        let plan = ecu_plan(
            &twins,
            &[("D", &[0, 1])],
            &[("c1", &[("e", false, &["D"])])],
        );
        let group = &plan.ecu_reads[0].groups[0];
        assert_eq!(group.requests.len(), 1);
        assert_eq!(group.requests[0].request, [0x22, 0xF1, 0x87]);
        assert_eq!(group.requests[0].declarations.len(), 2);
    }

    /// The report starts with every declaration not read (link refused for a refused document)
    /// and every entry not read; its ECU section reads back from the JSON.
    #[test]
    fn the_report_starts_with_every_declaration_not_read_and_every_entry_not_read() {
        let a = (0x7E0, 0x7E8);
        let refused = identified_by(0x87, 11, b"P", "v2", (0x7E1, 0x7E9), |p| {
            p.extended_addressing = true
        });
        let documents = [
            identified_by(0x87, 11, b"P", "v1", a, |_| {}),
            refused.clone(),
        ];
        let plan = ecu_plan(
            &documents,
            &[("D", &[0, 1])],
            &[("c1", &[("engine", false, &["D"])])],
        );
        let report = plan.report();
        let declared: Vec<_> = report
            .ecu_reads()
            .iter()
            .map(|read| {
                (
                    read.endpoint.is_some(),
                    read.requests[0].declarations[0].outcome.clone(),
                    read.matching.is_empty() && !read.conclusive && !read.answered,
                )
            })
            .collect();
        assert_eq!(
            declared,
            [
                (true, RequestOutcome::NotRead, true),
                (false, RequestOutcome::LinkRefused, true)
            ]
        );
        assert_eq!(report.ecu_reads()[1].documents, [refused.0]);
        assert!(!report.ecu_reads_interrupted());
        assert_eq!(report.entries().len(), 1);
        assert_eq!(report.entries()[0].state, EntryState::NotRead);
        assert_eq!(report.remaining(), report.candidates());
        assert!(report.dropped().is_empty() && report.read_order().is_empty());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["remaining"], serde_json::json!(["c1"]));
        assert_eq!(json["dropped"], serde_json::json!([]));
        assert_eq!(json["read_order"], serde_json::json!([]));
        assert_eq!(json["ecu_reads"][0]["requests"][0]["request"], "22f187");
        assert_eq!(
            json["ecu_reads"][0]["requests"][0]["declarations"][0]["outcome"],
            "NotRead"
        );
        assert!(json["ecu_reads"][1]["endpoint"].is_null());
        assert_eq!(json["entries"][0]["state"], "NotRead");
        assert_eq!(json["ecu_reads_interrupted"], false);
    }

    /// Every outcome and state shows in JSON by serde's default naming, with bytes and hashes as
    /// lowercase hex.
    #[test]
    fn the_ecu_section_serializes_with_bytes_as_hex() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut report = checked_reading(std::slice::from_ref(&doc))
            .unwrap()
            .report();
        let field = |value| IdentificationField {
            id: 1,
            short_name: "value".into(),
            value,
        };
        let read = &mut report.ecu_reads[0];
        read.requests[0].declarations = [
            RequestOutcome::Decoded(vec![
                field(FieldValue::Text("PA".into())),
                field(FieldValue::Bytes(vec![0xAB, 0x01])),
            ]),
            RequestOutcome::Negative(0x31),
            RequestOutcome::Undecodable,
            RequestOutcome::NoUsableAnswer,
            RequestOutcome::WorkerRefused,
            RequestOutcome::NotInPlay,
        ]
        .into_iter()
        .map(|outcome| DeclarationOutcome {
            document: doc.0,
            service: 1,
            pattern: true,
            outcome,
        })
        .collect();
        read.matching = vec![doc.0];
        read.answered = true;
        report.entries[0].state = EntryState::Identified {
            definition: "ecu".into(),
            variants: vec![doc.0],
        };
        let json = serde_json::to_value(&report).unwrap();
        let declarations = &json["ecu_reads"][0]["requests"][0]["declarations"];
        assert_eq!(declarations[0]["document"], hex(&doc.0));
        assert_eq!(
            declarations[0]["outcome"],
            serde_json::json!({"Decoded": [
                {"id": 1, "short_name": "value", "value": {"Text": "PA"}},
                {"id": 1, "short_name": "value", "value": {"Bytes": "ab01"}}
            ]})
        );
        assert_eq!(
            declarations[1]["outcome"],
            serde_json::json!({"Negative": 49})
        );
        assert_eq!(declarations[2]["outcome"], "Undecodable");
        assert_eq!(declarations[3]["outcome"], "NoUsableAnswer");
        assert_eq!(declarations[4]["outcome"], "WorkerRefused");
        assert_eq!(declarations[5]["outcome"], "NotInPlay");
        report.dropped.push(DroppedCandidate {
            candidate: "c1".into(),
            after: Some("D2".into()),
            contradictions: vec![
                Contradiction::UnlistedDefinition {
                    definition: "D2".into(),
                    endpoint: endpoint(0x7E0),
                    ruled_out: vec!["D1".into()],
                },
                Contradiction::VinRules {
                    component: Component::ModelYear,
                },
            ],
        });
        report.read_order.push(ReadStep {
            definition: "D1".into(),
            worst_case: 2,
        });
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["read_order"][0]["worst_case"], 2);
        assert_eq!(json["dropped"][0]["after"], "D2");
        assert_eq!(
            json["dropped"][0]["contradictions"][0]["UnlistedDefinition"]["ruled_out"],
            serde_json::json!(["D1"])
        );
        assert_eq!(
            json["dropped"][0]["contradictions"][0]["UnlistedDefinition"]["endpoint"]["tx_id"],
            0x7E0
        );
        assert_eq!(
            json["dropped"][0]["contradictions"][1],
            serde_json::json!({"VinRules": {"component": "ModelYear"}})
        );
        assert_eq!(json["ecu_reads"][0]["matching"][0], hex(&doc.0));
        assert_eq!(json["ecu_reads"][0]["answered"], true);
        assert_eq!(
            json["entries"][0]["state"],
            serde_json::json!({"Identified": {"definition": "ecu", "variants": [hex(&doc.0)]}})
        );
        for state in [
            EntryState::Ambiguous {
                definitions: vec!["a".into(), "b".into()],
            },
            EntryState::UnrecognizedVariant {
                endpoints: vec![EndpointKey {
                    bus: bus(500_000),
                    tx_id: 1,
                    rx_id: 2,
                }],
            },
            EntryState::AnsweredNotIdentified,
            EntryState::NotAnswering,
            EntryState::NotFitted,
        ] {
            let json = serde_json::to_value(&state).unwrap();
            assert!(json.is_string() || json.is_object(), "{json}");
        }
        assert_eq!(
            serde_json::to_value(EntryState::Ambiguous {
                definitions: vec!["a".into()]
            })
            .unwrap(),
            serde_json::json!({"Ambiguous": {"definitions": ["a"]}})
        );
    }

    fn endpoint(tx: u32) -> EndpointKey {
        EndpointKey {
            bus: bus(500_000),
            tx_id: tx,
            rx_id: tx + 8,
        }
    }

    /// A read of `definition` at the endpoint of `tx` (none for a refused link) with one
    /// declaration that gave `outcome`.
    fn read_of(
        definition: &str,
        tx: Option<u32>,
        outcome: RequestOutcome,
        matching: bool,
        conclusive: bool,
    ) -> EcuRead {
        let answered = matches!(
            outcome,
            RequestOutcome::Decoded(_) | RequestOutcome::Undecodable | RequestOutcome::Negative(_)
        );
        EcuRead {
            definition: definition.into(),
            endpoint: tx.map(endpoint),
            documents: vec![[1; 32]],
            requests: vec![IdentificationRequest {
                request: vec![0x22, 0xF1, 0x87],
                declarations: vec![DeclarationOutcome {
                    document: [1; 32],
                    service: 1,
                    pattern: true,
                    outcome,
                }],
            }],
            matching: if matching { vec![[1; 32]] } else { Vec::new() },
            conclusive,
            answered,
        }
    }

    fn entry_of(optional: bool, alternatives: &[&str]) -> EntryResult {
        EntryResult {
            candidate: "c1".into(),
            role: "engine".into(),
            optional,
            alternatives: alternatives.iter().map(|a| (*a).into()).collect(),
            state: EntryState::NotRead,
        }
    }

    fn state_of(optional: bool, alternatives: &[&str], reads: &[EcuRead]) -> EntryState {
        let mut entries = [entry_of(optional, alternatives)];
        evaluate_entries(&mut entries, reads);
        let [entry] = entries;
        entry.state
    }

    /// ADR-326 item 3 and ADR-327 item 3 as ADR-401 reads them, over hand-built reads.
    #[test]
    fn entry_states_follow_the_reads_of_the_alternatives() {
        let decoded = || RequestOutcome::Decoded(Vec::new());
        let both = ["D1", "D2"];
        // One alternative with a matching variant identifies the entry; an inconclusive
        // alternative beside it does not matter.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), true, true),
            read_of(
                "D2",
                Some(0x7E0),
                RequestOutcome::Negative(0x31),
                false,
                false,
            ),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::Identified {
                definition: "D1".into(),
                variants: vec![[1; 32]]
            }
        );
        // Two matching alternatives make it ambiguous.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), true, true),
            read_of("D2", Some(0x7E0), decoded(), true, true),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::Ambiguous {
                definitions: vec!["D1".into(), "D2".into()]
            }
        );
        // Answered, every read there conclusive, no match.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            read_of("D2", Some(0x7E0), decoded(), false, true),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::UnrecognizedVariant {
                endpoints: vec![endpoint(0x7E0)]
            }
        );
        // One alternative inconclusive: answered but not identified.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            read_of(
                "D2",
                Some(0x7E0),
                RequestOutcome::Negative(0x31),
                false,
                false,
            ),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::AnsweredNotIdentified
        );
        // Two endpoints, one answered and settled and one answered and not: not identified.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            read_of("D1", Some(0x7E1), RequestOutcome::Undecodable, false, false),
        ];
        assert_eq!(
            state_of(false, &["D1"], &reads),
            EntryState::AnsweredNotIdentified
        );
        // An endpoint that did not answer does not unsettle the one that did.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            read_of(
                "D1",
                Some(0x7E1),
                RequestOutcome::NoUsableAnswer,
                false,
                false,
            ),
        ];
        assert_eq!(
            state_of(false, &["D1"], &reads),
            EntryState::UnrecognizedVariant {
                endpoints: vec![endpoint(0x7E0)]
            }
        );
        // A variant whose link is refused leaves the definition inconclusive at its endpoints.
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, false),
            read_of("D1", None, RequestOutcome::LinkRefused, false, false),
        ];
        assert_eq!(
            state_of(false, &["D1"], &reads),
            EntryState::AnsweredNotIdentified
        );
        // Sent, nothing answered: not answering, or not fitted when optional.
        let silent = [read_of(
            "D1",
            Some(0x7E0),
            RequestOutcome::NoUsableAnswer,
            false,
            false,
        )];
        assert_eq!(state_of(false, &["D1"], &silent), EntryState::NotAnswering);
        assert_eq!(state_of(true, &["D1"], &silent), EntryState::NotFitted);
        // Nothing sent: not read, whether never run, refused by the link or by the worker.
        for outcome in [
            RequestOutcome::NotRead,
            RequestOutcome::LinkRefused,
            RequestOutcome::WorkerRefused,
        ] {
            let reads = [read_of("D1", Some(0x7E0), outcome, false, false)];
            assert_eq!(state_of(false, &["D1"], &reads), EntryState::NotRead);
        }
        // Reads of definitions the entry does not list do not count.
        let reads = [read_of("Z", Some(0x7E0), decoded(), true, true)];
        assert_eq!(state_of(false, &["D1"], &reads), EntryState::NotRead);
    }

    /// An alternative with no physical read makes the entry not read, whatever the others show;
    /// a refused-link alternative beside a conclusive read without a match is inconclusive
    /// (ADR-401).
    #[test]
    fn an_unread_or_refused_alternative_decides_the_entry() {
        let decoded = || RequestOutcome::Decoded(Vec::new());
        let both = ["D1", "D2"];
        for unread in [RequestOutcome::NotRead, RequestOutcome::WorkerRefused] {
            let reads = [
                read_of("D1", Some(0x7E0), decoded(), true, true),
                read_of("D2", Some(0x7E1), unread, false, false),
            ];
            assert_eq!(state_of(false, &both, &reads), EntryState::NotRead);
        }
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            read_of("D2", None, RequestOutcome::LinkRefused, false, false),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::AnsweredNotIdentified
        );
        // A refused variant with no request (no pattern) counts for nothing.
        let mut bare = read_of("D2", None, RequestOutcome::LinkRefused, false, false);
        bare.requests.clear();
        let reads = [read_of("D1", Some(0x7E0), decoded(), false, true), bare];
        assert_eq!(
            state_of(false, &["D1", "D2"], &reads),
            EntryState::UnrecognizedVariant {
                endpoints: vec![endpoint(0x7E0)]
            }
        );
        // A pattern-less alternative is skipped, so another one's match decides.
        let mut bare = read_of("D2", Some(0x7E1), decoded(), false, false);
        bare.requests.clear();
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), true, true),
            bare.clone(),
        ];
        assert!(matches!(
            state_of(false, &both, &reads),
            EntryState::Identified { .. }
        ));
        // A pattern-less alternative at the same endpoint does not keep a conclusive
        // no-match from settling it.
        let mut bare = read_of("D2", Some(0x7E0), decoded(), false, false);
        bare.requests.clear();
        let reads = [
            read_of("D1", Some(0x7E0), decoded(), false, true),
            bare.clone(),
        ];
        assert_eq!(
            state_of(false, &both, &reads),
            EntryState::UnrecognizedVariant {
                endpoints: vec![endpoint(0x7E0)]
            }
        );
        // All alternatives pattern-less: nothing was read.
        assert_eq!(state_of(false, &["D2"], &[bare]), EntryState::NotRead);
    }

    /// A decoded field can hold the VIN: `Debug` of the report shows no value, the JSON does.
    #[test]
    fn debug_hides_a_decoded_field_value() {
        let doc = named(document(0x90, 0x7E0, 0x7E8, |_| {}));
        let mut report = checked_reading(std::slice::from_ref(&doc))
            .unwrap()
            .report();
        let vin = "WVWZZZ1JZ3W386752";
        let field = |value| IdentificationField {
            id: 1,
            short_name: "value".into(),
            value,
        };
        report.ecu_reads[0].requests[0].declarations[0].outcome = RequestOutcome::Decoded(vec![
            field(FieldValue::Text(vin.into())),
            field(FieldValue::Bytes(vin.as_bytes().to_vec())),
            field(FieldValue::Group(vec![field(FieldValue::Unsigned(
                0x5757_575A,
            ))])),
        ]);
        let debug = format!("{report:?}");
        assert!(
            !debug.contains("WVWZZZ") && !debug.contains("5757"),
            "{debug}"
        );
        assert!(debug.contains("Text(<17 bytes>)"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(vin));
    }

    // ------------------------------------------------------ Narrowing and the next ECU (ADR-402)

    fn fact(definition: &str, tx: u32, verdict: Verdict) -> Fact {
        Fact {
            definition: definition.into(),
            endpoint: endpoint(tx),
            verdict,
        }
    }

    fn ids(set: &[&str]) -> Vec<String> {
        set.iter().map(|id| (*id).to_owned()).collect()
    }

    fn vin_rule(candidate: &str, component: Component, outcome: RuleOutcome) -> VinRuleResult {
        VinRuleResult {
            candidate: candidate.into(),
            component,
            value: "2020".into(),
            start: 10,
            end: 10,
            outcome,
        }
    }

    /// Two candidates whose engines are definitions at one endpoint, and a second ECU only the
    /// second candidate has.
    fn two_engines() -> CheckedPlan {
        let (a, b) = ((0x7E0, 0x7E8), (0x7E1, 0x7E9));
        let documents = [
            identified_by(0x87, 11, b"P", "v1", a, |_| {}),
            identified_by(0x87, 11, b"Q", "v2", a, |_| {}),
            identified_by(0x87, 11, b"P", "v3", b, |_| {}),
        ];
        ecu_plan(
            &documents,
            &[("D1", &[0]), ("D2", &[1]), ("D3", &[2])],
            &[
                ("c1", &[("engine", false, &["D1"])]),
                (
                    "c2",
                    &[("engine", false, &["D2"]), ("body", false, &["D3"])],
                ),
            ],
        )
    }

    /// ADR-327 item 3 and ADR-402: an identified definition a candidate lists in no entry
    /// contradicts it only at an endpoint one of its entries uses and only once every definition
    /// it lists there is ruled out; a listed definition that matched, an unused endpoint and a
    /// definition whose read stayed inconclusive contradict nothing; the VIN rules contradict per
    /// component and only when all of the component's rules are known not to match.
    #[test]
    fn a_candidate_is_contradicted_only_by_an_unlisted_identified_definition_at_a_settled_endpoint()
    {
        use Verdict::{Identified, RuledOut};
        let plan = two_engines();
        let dropped = |facts: &[Fact]| -> Vec<(String, Vec<Contradiction>)> {
            contradictions(&plan, facts, &[], &[])
        };
        // The other candidate's definition identified, own one not read yet: nothing.
        assert!(dropped(&[fact("D2", 0x7E0, Identified)]).is_empty());
        // Own definition ruled out there: c1 is contradicted, c2 (which lists D2) is not.
        assert_eq!(
            dropped(&[fact("D1", 0x7E0, RuledOut), fact("D2", 0x7E0, Identified)]),
            [(
                "c1".to_owned(),
                vec![Contradiction::UnlistedDefinition {
                    definition: "D2".into(),
                    endpoint: endpoint(0x7E0),
                    ruled_out: ids(&["D1"]),
                }]
            )]
        );
        // The same, the other way: D1 identified, D2 ruled out contradicts c2.
        let facts = [fact("D1", 0x7E0, Identified), fact("D2", 0x7E0, RuledOut)];
        let found = dropped(&facts);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "c2");
        // Both matched: neither is contradicted.
        assert!(
            dropped(&[fact("D1", 0x7E0, Identified), fact("D2", 0x7E0, Identified)]).is_empty()
        );
        // An endpoint c1 does not use.
        assert!(dropped(&[fact("D1", 0x7E0, RuledOut), fact("D3", 0x7E1, Identified)]).is_empty());
        // A listed definition ruled out at an endpoint nobody identified anything else at.
        assert!(dropped(&[fact("D3", 0x7E1, RuledOut)]).is_empty());
        // Ruled out facts alone contradict nothing.
        assert!(dropped(&[fact("D1", 0x7E0, RuledOut), fact("D2", 0x7E0, RuledOut)]).is_empty());

        // The VIN rules: per component, all not matched.
        use Component::{ModelYear, Oem};
        use RuleOutcome::{Matched, NotMatched, Unevaluated};
        let by_vin = |rules: &[VinRuleResult]| contradictions(&plan, &[], rules, &[]);
        assert_eq!(
            by_vin(&[vin_rule("c1", ModelYear, NotMatched)]),
            [(
                "c1".to_owned(),
                vec![Contradiction::VinRules {
                    component: ModelYear
                }]
            )]
        );
        assert!(
            by_vin(&[
                vin_rule("c1", ModelYear, NotMatched),
                vin_rule("c1", ModelYear, Matched)
            ])
            .is_empty()
        );
        assert!(by_vin(&[vin_rule("c1", ModelYear, Unevaluated)]).is_empty());
        let found = by_vin(&[
            vin_rule("c2", ModelYear, Matched),
            vin_rule("c2", Oem, NotMatched),
        ]);
        assert_eq!(
            found,
            [(
                "c2".to_owned(),
                vec![Contradiction::VinRules { component: Oem }]
            )]
        );
        // A VIN contradiction is listed before the facts'.
        let both = contradictions(
            &plan,
            &[fact("D1", 0x7E0, RuledOut), fact("D2", 0x7E0, Identified)],
            &[vin_rule("c1", ModelYear, NotMatched)],
            &[],
        );
        assert_eq!(both[0].1.len(), 2);
        assert!(matches!(both[0].1[0], Contradiction::VinRules { .. }));
    }

    /// ADR-327 item 4 and ADR-402: the outcomes of a read are the conclusive results per endpoint,
    /// each judged by the same `contradictions` as the run; a definition with a refused variant
    /// cannot be ruled out, one with no request has none, and the next definition follows the
    /// tie-breaks.
    #[test]
    fn outcomes_are_the_conclusive_results_per_endpoint() {
        use Verdict::RuledOut;
        let plan = two_engines();
        let all = ids(&["c1", "c2"]);
        let ruled = [fact("D1", 0x7E0, RuledOut)];
        assert_eq!(
            outcome_sets(&plan, &ruled, &[], &[], &all, "D2"),
            [ids(&["c2"]), ids(&["c1", "c2"])]
        );
        // Without that fact no outcome of D2 drops c1.
        assert_eq!(
            outcome_sets(&plan, &[], &[], &[], &all, "D2"),
            [ids(&["c1", "c2"])]
        );
        // D3 has one endpoint nobody else uses: nothing drops whatever it shows.
        assert_eq!(
            outcome_sets(&plan, &ruled, &[], &[], &all, "D3"),
            [ids(&["c1", "c2"])]
        );
        // A refused variant: only the identified outcome.
        let (a, b) = ((0x7E0, 0x7E8), (0x7E1, 0x7E9));
        let documents = [
            identified_by(0x87, 11, b"P", "v1", a, |_| {}),
            identified_by(0x87, 11, b"P", "v2", a, |_| {}),
            identified_by(0x87, 11, b"P", "v3", b, |p| p.extended_addressing = true),
        ];
        let refused = ecu_plan(
            &documents,
            &[("D1", &[0]), ("D2", &[1, 2])],
            &[
                ("c1", &[("engine", false, &["D1"])]),
                ("c2", &[("engine", false, &["D2"])]),
            ],
        );
        assert_eq!(
            outcome_sets(&refused, &ruled, &[], &[], &all, "D2"),
            [ids(&["c2"])]
        );
        // No pattern: no request, so no outcome, and the definition is never picked.
        let bare = named(edited(&documents[0].1, |v| v.patterns = None));
        let barren = ecu_plan(
            &[bare, documents[1].clone()],
            &[("D1", &[0]), ("D2", &[1])],
            &[
                ("c1", &[("engine", false, &["D1"])]),
                ("c2", &[("engine", false, &["D2"])]),
            ],
        );
        assert!(outcome_sets(&barren, &[], &[], &[], &all, "D1").is_empty());
        let report = barren.report();
        let picked = next_definition(&barren, &report, &all, &HashSet::new());
        assert_eq!(picked, Some(("D2".to_owned(), 2)));
    }

    /// ADR-327 item 4 and ADR-402: among definitions with the same worst case, the one named by
    /// the most remaining candidates, then the one with the most endpoints that answered presence,
    /// then the lowest identifier; tried definitions and definitions no remaining candidate names
    /// are not offered.
    #[test]
    fn the_next_definition_follows_the_tie_breaks() {
        let (a, b) = ((0x7E0, 0x7E8), (0x7E1, 0x7E9));
        let documents = [
            identified_by(0x87, 11, b"P", "v1", a, |_| {}),
            identified_by(0x87, 11, b"P", "v2", b, |_| {}),
            identified_by(0x87, 11, b"P", "v3", a, |_| {}),
        ];
        // Plan order Da, Dc, Db; Db is named by two candidates, Dc and Da by one.
        let plan = ecu_plan(
            &documents,
            &[("Da", &[0]), ("Dc", &[1]), ("Db", &[2])],
            &[
                ("c1", &[("x", false, &["Db"]), ("y", false, &["Dc"])]),
                ("c2", &[("x", false, &["Db"])]),
                ("c3", &[("x", false, &["Da"])]),
            ],
        );
        let all = ids(&["c1", "c2", "c3"]);
        let mut report = plan.report();
        let mut tried = HashSet::new();
        assert_eq!(
            next_definition(&plan, &report, &all, &tried),
            Some(("Db".to_owned(), 3)),
            "named by two beats the lower identifier and the plan order"
        );
        tried.insert("Db".to_owned());
        assert_eq!(
            next_definition(&plan, &report, &all, &tried).map(|d| d.0),
            Some("Da".to_owned()),
            "the lowest identifier when nothing else separates"
        );
        report.presence.push(PresenceAnswer {
            bus: bus(500_000),
            rx_id: b.1,
            bytes: vec![0x7E, 0],
        });
        assert_eq!(
            next_definition(&plan, &report, &all, &tried).map(|d| d.0),
            Some("Dc".to_owned()),
            "an endpoint that answered presence beats the lower identifier"
        );
        tried.insert("Dc".to_owned());
        // c3 gone: Da is named by no remaining candidate and is not offered.
        let left = ids(&["c1", "c2"]);
        assert_eq!(next_definition(&plan, &report, &left, &tried), None);
        tried.insert("Da".to_owned());
        assert_eq!(next_definition(&plan, &report, &all, &tried), None);
    }

    /// ADR-402: the fold over the endpoints gives the sets of the brute-force product of the
    /// alternatives (three endpoints, three candidates, differing drops), and a definition at many
    /// endpoints is folded at once.
    #[test]
    fn the_per_endpoint_fold_equals_the_product() {
        let all = ids(&["a", "b", "c"]);
        let endpoints = vec![
            vec![ids(&["a"]), ids(&[])],
            vec![ids(&["b"]), ids(&["a", "c"])],
            vec![ids(&[]), ids(&["b"]), ids(&["c"])],
        ];
        let mut brute: Vec<Vec<String>> = Vec::new();
        for x in &endpoints[0] {
            for y in &endpoints[1] {
                for z in &endpoints[2] {
                    let left: Vec<String> = all
                        .iter()
                        .filter(|c| !x.contains(c) && !y.contains(c) && !z.contains(c))
                        .cloned()
                        .collect();
                    if !brute.contains(&left) {
                        brute.push(left);
                    }
                }
            }
        }
        let mut folded = fold_outcomes(&all, &endpoints);
        folded.sort();
        brute.sort();
        assert_eq!(folded, brute);
        assert!(folded.len() > 1);
        // 24 endpoints with two alternatives each: 2^24 combinations, a few distinct sets.
        let many = vec![vec![ids(&["a"]), ids(&[])]; 24];
        let sets = fold_outcomes(&all, &many);
        assert_eq!(sets.iter().map(Vec::len).max(), Some(3));
        assert_eq!(sets.len(), 2);
        // Nothing to drop: one set, whatever the endpoints.
        assert_eq!(fold_outcomes(&[], &many), [Vec::<String>::new()]);
        assert!(fold_outcomes(&all, &[]).is_empty());
    }

    /// ADR-402: a definition read at 24 endpoints has its worst case computed at once.
    #[test]
    fn a_definition_at_many_endpoints_has_its_worst_case_computed_at_once() {
        let documents: Vec<_> = (0..24u32)
            .map(|n| identified_by(0x87, 11, b"P", "v", (0x600 + n, 0x700 + n), |_| {}))
            .collect();
        let indexes: Vec<usize> = (0..24).collect();
        let plan = ecu_plan(
            &documents,
            &[("D1", &indexes)],
            &[
                ("c1", &[("engine", false, &["D1"])]),
                ("c2", &[("engine", false, &["D1"])]),
            ],
        );
        let all = ids(&["c1", "c2"]);
        let sets = outcome_sets(&plan, &[], &[], &[], &all, "D1");
        assert_eq!(sets, [all]);
    }

    /// ADR-327 item 3, ADR-402: every definition the candidate lists at the endpoint must be ruled
    /// out: one ruled out and another inconclusive keep the candidate.
    #[test]
    fn a_candidate_is_kept_while_any_definition_it_lists_at_the_endpoint_is_not_ruled_out() {
        use Verdict::{Identified, RuledOut};
        let a = (0x7E0, 0x7E8);
        let documents = [
            identified_by(0x87, 11, b"Q", "v1", a, |_| {}),
            identified_by(0x87, 11, b"Q", "v5", a, |_| {}),
            identified_by(0x87, 11, b"P", "v2", a, |_| {}),
        ];
        let plan = ecu_plan(
            &documents,
            &[("D1", &[0]), ("D5", &[1]), ("D2", &[2])],
            &[
                ("c1", &[("x", false, &["D1", "D5"])]),
                ("c2", &[("y", false, &["D2"])]),
            ],
        );
        // D5 gave no fact (inconclusive).
        let facts = [fact("D1", 0x7E0, RuledOut), fact("D2", 0x7E0, Identified)];
        assert!(contradictions(&plan, &facts, &[], &[]).is_empty());
        let facts = [
            fact("D1", 0x7E0, RuledOut),
            fact("D5", 0x7E0, RuledOut),
            fact("D2", 0x7E0, Identified),
        ];
        let found = contradictions(&plan, &facts, &[], &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "c1");
    }

    /// `narrow` records a drop once, with the read that completed it, and keeps every candidate of
    /// a plan whose selection does not narrow.
    #[test]
    fn narrow_records_each_drop_once_and_a_manual_plan_drops_nothing() {
        let plan = two_engines();
        let mut report = plan.report();
        assert_eq!(report.remaining(), ["c1", "c2"]);
        assert!(report.dropped().is_empty() && report.read_order().is_empty());
        // D1 ruled out, D2 identified at the same endpoint.
        report.ecu_reads[0].conclusive = true;
        report.ecu_reads[1].matching = vec![[1; 32]];
        assert_eq!(narrow(&plan, &mut report, Some("D2")), ["c2"]);
        assert_eq!(narrow(&plan, &mut report, Some("D3")), ["c2"]);
        assert_eq!(report.remaining(), ["c2"]);
        assert_eq!(
            report.dropped(),
            [DroppedCandidate {
                candidate: "c1".into(),
                after: Some("D2".into()),
                contradictions: vec![Contradiction::UnlistedDefinition {
                    definition: "D2".into(),
                    endpoint: endpoint(0x7E0),
                    ruled_out: ids(&["D1"]),
                }],
            }]
        );
        let mut manual = two_engines();
        manual.selection = Chosen::ChosenDefinition;
        let mut report = manual.report();
        report.ecu_reads[0].conclusive = true;
        report.ecu_reads[1].matching = vec![[1; 32]];
        assert_eq!(narrow(&manual, &mut report, None), ["c1", "c2"]);
        assert!(report.dropped().is_empty());
        assert!(in_play(&manual, &[], "D1"));
        assert!(!in_play(&plan, &ids(&["c1"]), "D2"));
    }

    // ------------------------------------------------------ The rule fields (ADR-403)

    const YEAR: Component = Component::ModelYear;
    const VMODEL: Component = Component::VehicleModel;

    /// A document on `endpoint` whose service 1 reads the software version (F189) as five ASCII
    /// bytes, with a pattern on the text starting with "S".
    fn sw_doc(name: &str, endpoint: (u32, u32)) -> (Sha256, Vec<u8>) {
        identified_by(0x89, 5, b"S", name, endpoint, |_| {})
    }

    /// `document` with `edit` applied to the first field of service 1's positive response.
    fn with_field(
        document: &(Sha256, Vec<u8>),
        edit: impl FnOnce(&mut FieldT),
    ) -> (Sha256, Vec<u8>) {
        named(edited(&document.1, |variant| {
            let service = &mut variant.services.as_mut().unwrap()[0];
            edit(&mut service.positive.as_mut().unwrap().fields.as_mut().unwrap()[0]);
        }))
    }

    const D_ENGINE: &[(&str, bool, &[&str])] = &[("engine", false, &["D"])];

    /// A plan of one definition "D" over `documents`, every candidate listing it, with `rules`.
    fn rule_plan(
        documents: &[(Sha256, Vec<u8>)],
        candidates: &[&str],
        rules: &[CandidateFieldRule<'_>],
    ) -> Result<CheckedPlan, PlanRefused> {
        let all: Vec<usize> = (0..documents.len()).collect();
        let entries: Vec<CandidateEntries<'_>> =
            candidates.iter().map(|&id| (id, D_ENGINE)).collect();
        let selection = if candidates.len() == 1 {
            Chosen::ChosenDefinition
        } else {
            Chosen::Candidates
        };
        try_ecu_plan_with_rules(selection, documents, &[("D", &all)], &entries, &[], rules)
    }

    /// ADR-306 item 5 and ADR-403: a field rule whose source a pattern could not compare, whose
    /// expected value could never match, whose request needs a value, or whose rule field mixes
    /// kinds is refused before any link opens, naming the source; the ASCII rule of `plan()`
    /// passes.
    #[test]
    fn a_field_rule_a_pattern_could_not_compare_is_refused() {
        let refused = |documents: &[(Sha256, Vec<u8>)], expected: &[u8]| {
            let rule: CandidateFieldRule<'_> = ("c1", 0, 1, 1, YEAR, "2020", expected);
            match rule_plan(documents, &["c1"], &[rule]) {
                Err(PlanRefused::FieldSource {
                    document,
                    service,
                    field,
                    why,
                }) => {
                    assert_eq!((document, service, field), (hex(&documents[0].0), 1, 1));
                    why
                }
                other => panic!("not refused as a field source: {other:?}"),
            }
        };
        let text = sw_doc("v1", (0x7E0, 0x7E8));
        assert!(
            rule_plan(
                std::slice::from_ref(&text),
                &["c1"],
                &[("c1", 0, 1, 1, YEAR, "y", b"SW")]
            )
            .is_ok()
        );
        // A float, and a number under a linear conversion, which decodes to one.
        let float = with_field(&text, |f| f.kind = DataKind::Float);
        let why = refused(std::slice::from_ref(&float), b"SW");
        assert!(why.contains("could not compare"), "{why}");
        let linear = with_field(&text, |f| {
            f.kind = DataKind::Uint;
            let mut linear = CompuLinearT::default();
            linear.numerator = 1.0;
            linear.denominator = 2.0;
            f.compu = CompuT::CompuLinear(Box::new(linear));
        });
        assert!(refused(std::slice::from_ref(&linear), &[1]).contains("could not compare"));
        // An expected value no value matches: empty, or over eight bytes for an integer.
        assert!(refused(std::slice::from_ref(&text), b"").contains("empty"));
        let number = with_field(&text, |f| {
            f.kind = DataKind::Uint;
            f.bit_length = 8;
        });
        assert!(refused(std::slice::from_ref(&number), &[0; 9]).contains("eight bytes"));
        // The same nine bytes are a fine prefix of text.
        assert!(
            rule_plan(
                std::slice::from_ref(&text),
                &["c1"],
                &[("c1", 0, 1, 1, YEAR, "y", b"SW1234567")]
            )
            .is_ok()
        );
        // A request that needs a value, on a service only the rule names.
        let rule_only = with_service(&sw_doc("v1", (0x7E0, 0x7E8)), {
            let mut service = read_service(2, "readX", [0xF1, 0x90], DataKind::Ascii, 5);
            let mut argument = FieldT::default();
            argument.id = 9;
            argument.placement = Placement::AfterPrevious;
            argument.bit_length = 8;
            argument.kind = DataKind::Uint;
            service.request.as_mut().unwrap().params = Some(vec![argument]);
            service
        });
        let rule: CandidateFieldRule<'_> = ("c1", 0, 2, 1, YEAR, "2020", b"S");
        assert!(matches!(
            rule_plan(std::slice::from_ref(&rule_only), &["c1"], &[rule]),
            Err(PlanRefused::FieldSource {
                service: 2,
                field: 1,
                ..
            })
        ));
        // Sources of one component on one request that read their value by different kinds.
        let both = with_service(
            &text,
            read_service(2, "readNum", [0xF1, 0x89], DataKind::Uint, 1),
        );
        let rules: [CandidateFieldRule<'_>; 2] = [
            ("c1", 0, 1, 1, YEAR, "a", b"S"),
            ("c1", 0, 2, 1, YEAR, "b", &[1]),
        ];
        match rule_plan(std::slice::from_ref(&both), &["c1"], &rules) {
            Err(PlanRefused::FieldSource { why, .. }) => assert!(why.contains("different kinds")),
            other => panic!("{other:?}"),
        }
        // Another component on that request is another rule field: accepted.
        let rules: [CandidateFieldRule<'_>; 2] = [
            ("c1", 0, 1, 1, YEAR, "a", b"S"),
            ("c1", 0, 2, 1, VMODEL, "b", &[1]),
        ];
        assert!(rule_plan(std::slice::from_ref(&both), &["c1"], &rules).is_ok());
    }

    /// ADR-403 item 1: a rule on a pattern service adds no declaration; a rule on another service
    /// is a declaration of the request it builds, flagged as no pattern's, in its document's link
    /// group; two candidates sharing a source share its entry in the rule field; a rule on a
    /// second link group of the endpoint is a rule field of its own.
    #[test]
    fn rule_sources_join_their_definitions_link_groups_and_requests() {
        let a = (0x7E0, 0x7E8);
        let block = |size: i64| {
            move |p: &mut ProtocolT| {
                p.functional_id = 0;
                with_param("CP_BlockSize", size)(p);
            }
        };
        let first = with_service(
            &identified_by(0x89, 5, b"S", "v1", a, block(8)),
            read_service(2, "readPart", [0xF1, 0x87], DataKind::Ascii, 11),
        );
        // A second document of the definition on the same endpoint with another non-timing
        // ComParam, so a group of its own.
        let second = identified_by(0x89, 5, b"S", "v2", a, block(16));
        let rules: [CandidateFieldRule<'_>; 4] = [
            ("c1", 0, 1, 1, YEAR, "2021", b"SW1"),
            ("c1", 0, 2, 1, VMODEL, "m", b"PA"),
            ("c2", 0, 1, 1, YEAR, "2022", b"SW2"),
            ("c2", 1, 1, 1, YEAR, "2022", b"SW2"),
        ];
        let plan = rule_plan(&[first.clone(), second.clone()], &["c1", "c2"], &rules).unwrap();
        assert_eq!(plan.rule_fields.len(), 3);
        // Group 0 holds F189 (pattern, from service 1) and F187 (rule only, service 2).
        let group = &plan.ecu_reads[0].groups[0];
        let declared: Vec<_> = group
            .requests
            .iter()
            .map(|r| {
                (
                    r.request.clone(),
                    r.declarations
                        .iter()
                        .map(|d| (d.service, d.pattern))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            declared,
            [
                (vec![0x22, 0xF1, 0x89], vec![(1, true)]),
                (vec![0x22, 0xF1, 0x87], vec![(2, false)]),
            ]
        );
        // Two candidates sharing a source share its entry, and the ModelYear sources of group 0
        // and group 1 are two rule fields.
        let sources: Vec<(usize, usize, usize, Component, usize)> = plan
            .rule_fields
            .iter()
            .map(|f| (f.read, f.group, f.request, f.component, f.sources.len()))
            .collect();
        assert_eq!(
            sources,
            [(0, 0, 0, YEAR, 1), (0, 0, 1, VMODEL, 1), (0, 1, 0, YEAR, 1)]
        );
        assert!(plan.ecu_reads[0].groups.len() == 2);
        // The report lists the rule fields in that order with every rule on them.
        let report = plan.report();
        let shown: Vec<(&str, Vec<&str>)> = report
            .rule_fields()
            .iter()
            .map(|f| {
                (
                    if f.component == YEAR {
                        "ModelYear"
                    } else {
                        "VehicleModel"
                    },
                    f.rules.iter().map(|r| r.candidate.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("ModelYear", vec!["c1", "c2"]),
                ("VehicleModel", vec!["c1"]),
                ("ModelYear", vec!["c2"]),
            ]
        );
    }

    /// ADR-403: the report starts with every rule field's sources not read (or their link refused),
    /// no value established and every rule unevaluated, and shows requests and expected values as
    /// hex, the established value by its kind and a `FieldRules` contradiction with the field it
    /// names; a declaration shows whether a pattern names it.
    #[test]
    fn the_rule_fields_start_not_read_and_serialize_with_bytes_as_hex() {
        let refused = sw_doc_with("v2", (0x7E1, 0x7E9), |p| p.extended_addressing = true);
        let documents = [sw_doc("v1", (0x7E0, 0x7E8)), refused];
        let rules: [CandidateFieldRule<'_>; 2] = [
            ("c1", 0, 1, 1, YEAR, "2021", b"SW"),
            ("c1", 1, 1, 1, VMODEL, "m", b"S"),
        ];
        let plan = rule_plan(&documents, &["c1"], &rules).unwrap();
        let mut report = plan.report();
        assert!(report.component_values().is_empty());
        let fields = report.rule_fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].sources[0].outcome, RuleSourceOutcome::NotRead);
        assert_eq!(fields[1].sources[0].outcome, RuleSourceOutcome::LinkRefused);
        assert!(fields[1].endpoint.is_none() && fields[0].endpoint.is_some());
        assert!(fields.iter().all(|f| {
            f.established.is_none()
                && f.rules
                    .iter()
                    .all(|r| r.outcome == RuleOutcome::Unevaluated)
        }));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["component_values"], serde_json::json!([]));
        let field = &json["rule_fields"][0];
        assert_eq!(field["definition"], "D");
        assert_eq!(field["request"], "22f189");
        assert_eq!(field["component"], "ModelYear");
        assert_eq!(field["sources"][0]["outcome"], "NotRead");
        assert_eq!(field["sources"][0]["document"], hex(&documents[0].0));
        assert!(field["established"].is_null());
        assert_eq!(field["rules"][0]["expected"], "5357");
        assert_eq!(field["rules"][0]["outcome"], "Unevaluated");
        assert!(json["rule_fields"][1]["endpoint"].is_null());
        assert_eq!(
            json["rule_fields"][1]["sources"][0]["outcome"],
            "LinkRefused"
        );
        assert_eq!(
            json["ecu_reads"][0]["requests"][0]["declarations"][0]["pattern"],
            true
        );
        // Judged: the value shows by its kind, the rule's outcome by name.
        report.rule_fields[0].established = Some(FieldValue::Text("SW123".into()));
        report.rule_fields[0].sources[0].outcome =
            RuleSourceOutcome::Decoded(FieldValue::Text("SW123".into()));
        report.rule_fields[0].rules[0].outcome = RuleOutcome::NotMatched;
        report.rule_fields[1].sources[0].outcome = RuleSourceOutcome::Negative(0x31);
        report.component_values = vec![ComponentValue {
            component: YEAR,
            value: "2021".into(),
        }];
        report.dropped.push(DroppedCandidate {
            candidate: "c1".into(),
            after: Some("D".into()),
            contradictions: vec![Contradiction::FieldRules {
                component: YEAR,
                definition: "D".into(),
                endpoint: endpoint(0x7E0),
                group: 0,
                request: vec![0x22, 0xF1, 0x89],
            }],
        });
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["rule_fields"][0]["established"],
            serde_json::json!({"Text": "SW123"})
        );
        assert_eq!(
            json["rule_fields"][0]["sources"][0]["outcome"],
            serde_json::json!({"Decoded": {"Text": "SW123"}})
        );
        assert_eq!(
            json["rule_fields"][1]["sources"][0]["outcome"],
            serde_json::json!({"Negative": 49})
        );
        assert_eq!(json["rule_fields"][0]["rules"][0]["outcome"], "NotMatched");
        assert_eq!(
            json["component_values"],
            serde_json::json!([{"component": "ModelYear", "value": "2021"}])
        );
        let contradiction = &json["dropped"][0]["contradictions"][0]["FieldRules"];
        assert_eq!(contradiction["component"], "ModelYear");
        assert_eq!(contradiction["definition"], "D");
        assert_eq!(contradiction["request"], "22f189");
        assert_eq!(contradiction["endpoint"]["tx_id"], 0x7E0);
        // `Debug` hides an established value as it hides a decoded field.
        assert!(!format!("{report:?}").contains("SW123"));
    }

    /// `sw_doc` with `setup` applied to its protocol.
    fn sw_doc_with(
        name: &str,
        endpoint: (u32, u32),
        setup: impl FnOnce(&mut ProtocolT),
    ) -> (Sha256, Vec<u8>) {
        identified_by(0x89, 5, b"S", name, endpoint, setup)
    }

    /// ADR-403: per component and rule field, a candidate with rules on an established field none
    /// of which matched is contradicted, naming the field; one matched rule keeps it, an
    /// unestablished field drops nothing, another field of the component neither saves nor drops
    /// it, and the contradictions are listed VIN rules, field rules, then unlisted definitions.
    #[test]
    fn a_candidate_is_contradicted_by_a_rule_field_established_outside_its_values() {
        let a = (0x7E0, 0x7E8);
        let documents = [with_service(
            &sw_doc("v1", a),
            read_service(2, "readPart", [0xF1, 0x87], DataKind::Ascii, 11),
        )];
        let rules: [CandidateFieldRule<'_>; 3] = [
            ("c1", 0, 1, 1, YEAR, "2021", b"SW1"),
            ("c1", 0, 1, 1, YEAR, "2022", b"SW2"),
            ("c1", 0, 2, 1, YEAR, "2023", b"PA"),
        ];
        let plan = rule_plan(&documents, &["c1", "c2"], &rules).unwrap();
        let blank = plan.report().rule_fields;
        assert_eq!(blank.len(), 2);
        let judged = |first: &[RuleOutcome], second: &[RuleOutcome], established: [bool; 2]| {
            let mut fields = blank.clone();
            for (field, (outcomes, established)) in fields
                .iter_mut()
                .zip([first, second].into_iter().zip(established))
            {
                field.established = established.then(|| FieldValue::Text("x".into()));
                for (rule, outcome) in field.rules.iter_mut().zip(outcomes) {
                    rule.outcome = *outcome;
                }
            }
            fields
        };
        use RuleOutcome::{Matched, NotMatched, Unevaluated};
        let field_rules = |fields: &[RuleFieldRead]| contradictions(&plan, &[], &[], fields);
        let own = |request: &[u8]| {
            vec![(
                "c1".to_owned(),
                vec![Contradiction::FieldRules {
                    component: YEAR,
                    definition: "D".into(),
                    endpoint: endpoint(0x7E0),
                    group: 0,
                    request: request.to_vec(),
                }],
            )]
        };
        // None of the rules on the field matched: contradicted, naming the field.
        let none = judged(&[NotMatched, NotMatched], &[Matched], [true, true]);
        assert_eq!(field_rules(&none), own(&[0x22, 0xF1, 0x89]));
        // One matched: kept, whatever the other field shows.
        let one = judged(&[NotMatched, Matched], &[Matched], [true, true]);
        assert!(field_rules(&one).is_empty());
        // Not established: nothing, however the rules read.
        let unknown = judged(&[NotMatched, NotMatched], &[Unevaluated], [false, false]);
        assert!(field_rules(&unknown).is_empty());
        // The second field of the component alone contradicts, though the first one matched.
        let second = judged(&[Matched, NotMatched], &[NotMatched], [true, true]);
        assert_eq!(field_rules(&second), own(&[0x22, 0xF1, 0x87]));
        // A candidate with no rule on an established field is not contradicted by it.
        assert!(
            field_rules(&none)
                .iter()
                .all(|(candidate, _)| candidate == "c1")
        );
        // The order of the contradictions of one candidate.
        let documents = [
            sw_doc("v1", a),
            identified_by(0x87, 11, b"Q", "v2", a, |_| {}),
        ];
        let rules: [CandidateFieldRule<'_>; 1] = [("c1", 0, 1, 1, YEAR, "2021", b"SW1")];
        let entries: [CandidateEntries<'_>; 2] = [
            ("c1", &[("engine", false, &["D"])]),
            ("c2", &[("engine", false, &["E"])]),
        ];
        let plan = ecu_plan_with_rules(
            Chosen::Candidates,
            &documents,
            &[("D", &[0]), ("E", &[1])],
            &entries,
            &[],
            &rules,
        );
        let mut fields = plan.report().rule_fields;
        fields[0].established = Some(FieldValue::Text("x".into()));
        fields[0].rules[0].outcome = NotMatched;
        let facts = [
            fact("D", 0x7E0, Verdict::RuledOut),
            fact("E", 0x7E0, Verdict::Identified),
        ];
        let vin = [vin_rule("c1", VMODEL, NotMatched)];
        let found = contradictions(&plan, &facts, &vin, &fields);
        assert_eq!(found.len(), 1);
        let kinds: Vec<&str> = found[0]
            .1
            .iter()
            .map(|c| match c {
                Contradiction::VinRules { .. } => "vin",
                Contradiction::FieldRules { .. } => "field",
                Contradiction::UnlistedDefinition { .. } => "unlisted",
            })
            .collect();
        assert_eq!(kinds, ["vin", "field", "unlisted"]);
    }

    /// ADR-327 item 4, ADR-403: the classes of a rule field are the distinct values the remaining
    /// candidates expect, built by the field's kind, each with the candidates it drops, and the
    /// value no rule matches: a prefix of another expected value lands in one class with it, an
    /// integer by its number, whatever width it is written in; a text field's classes are bytes.
    #[test]
    fn rule_field_outcomes_group_candidates_whose_expected_values_overlap() {
        let a = (0x7E0, 0x7E8);
        let all = ids(&["c1", "c2"]);
        let sets = |plan: &CheckedPlan, remaining: &[String]| {
            let report = plan.report();
            let mut sets = outcome_sets(plan, &[], &[], &report.rule_fields, remaining, "D");
            for set in &mut sets {
                set.sort();
            }
            sets.sort();
            sets
        };
        // "SW" is a prefix of "SW1": the value "SW" satisfies c1 only, "SW1" satisfies both.
        let text = [sw_doc("v1", a)];
        let rules: [CandidateFieldRule<'_>; 2] = [
            ("c1", 0, 1, 1, YEAR, "a", b"SW"),
            ("c2", 0, 1, 1, YEAR, "b", b"SW1"),
        ];
        let plan = rule_plan(&text, &["c1", "c2"], &rules).unwrap();
        let report = plan.report();
        assert_eq!(
            rule_classes(&plan, &[], &[], &report.rule_fields, 0, &all),
            [ids(&["c2"]), ids(&[]), ids(&["c1", "c2"])],
            "the values SW, SW1 and the one no rule matches"
        );
        assert_eq!(
            sets(&plan, &all),
            [ids(&[]), ids(&["c1"]), ids(&["c1", "c2"])]
        );
        let worst = sets(&plan, &all).iter().map(Vec::len).max();
        assert_eq!(worst, Some(2));
        // A candidate that has left the remaining set gives no class of its own.
        assert_eq!(
            rule_classes(&plan, &[], &[], &report.rule_fields, 0, &ids(&["c1"])).len(),
            2
        );
        // Integers: [1] and [0, 1] are the one number 1.
        let number = with_field(&sw_doc("v1", a), |f| {
            f.kind = DataKind::Uint;
            f.bit_length = 8;
        });
        let rules: [CandidateFieldRule<'_>; 3] = [
            ("c1", 0, 1, 1, YEAR, "a", &[1]),
            ("c2", 0, 1, 1, YEAR, "b", &[0, 1]),
            ("c3", 0, 1, 1, YEAR, "c", &[2]),
        ];
        let plan = rule_plan(&[number], &["c1", "c2", "c3"], &rules).unwrap();
        let report = plan.report();
        let every = ids(&["c1", "c2", "c3"]);
        assert_eq!(
            rule_classes(&plan, &[], &[], &report.rule_fields, 0, &every),
            [ids(&["c3"]), ids(&["c1", "c2"]), ids(&["c1", "c2", "c3"])]
        );
        // A plan whose selection does not narrow has the one class that drops nothing.
        let mut manual = rule_plan(&text, &["c1"], &rules[..1]).unwrap();
        manual.selection = Chosen::ChosenDefinition;
        let report = manual.report();
        assert_eq!(
            rule_classes(&manual, &[], &[], &report.rule_fields, 0, &ids(&["c1"])),
            [Vec::<String>::new()]
        );
    }

    /// ADR-403: an unevaluated rule field is not an outcome, at any endpoint, so a definition read
    /// at two endpoints whose rule fields separate every candidate has the worst case 1, not the
    /// remaining count the mixed outcome would give.
    #[test]
    fn an_unevaluated_rule_field_is_not_an_outcome() {
        let documents = [sw_doc("v1", (0x7E0, 0x7E8)), sw_doc("v1b", (0x7E1, 0x7E9))];
        let mut rules: Vec<CandidateFieldRule<'_>> = Vec::new();
        for (candidate, value, expected) in [
            ("c1", "1", &b"SW1"[..]),
            ("c2", "2", &b"SW2"[..]),
            ("c3", "3", &b"SW3"[..]),
        ] {
            for document in 0..2 {
                rules.push((candidate, document, 1, 1, YEAR, value, expected));
            }
        }
        let plan = rule_plan(&documents, &["c1", "c2", "c3"], &rules).unwrap();
        assert_eq!(plan.rule_fields.len(), 2);
        let report = plan.report();
        let all = ids(&["c1", "c2", "c3"]);
        let sets = outcome_sets(&plan, &[], &[], &report.rule_fields, &all, "D");
        assert!(!sets.is_empty());
        assert_eq!(sets.iter().map(Vec::len).max(), Some(1), "{sets:?}");
        // Only the conclusive combinations: one candidate, or none, each class of each field.
        assert!(sets.contains(&ids(&["c1"])) && sets.contains(&ids(&[])));
    }

    /// ADR-306 item 5 and ADR-403: a field rule whose expected value no value of its field can
    /// equal is refused, naming the source: a number the field's fixed width cannot hold (an
    /// unsigned, signed or BCD field), a value longer than a fixed-length bytes or text field can
    /// be, and text that is not UTF-8; the boundary values pass.
    #[test]
    fn a_field_rule_that_could_never_match_is_refused() {
        let base = sw_doc("v1", (0x7E0, 0x7E8));
        let outcome = |edit: &dyn Fn(&mut FieldT), expected: &[u8]| {
            let document = with_field(&base, |field| edit(field));
            let rule: CandidateFieldRule<'_> = ("c1", 0, 1, 1, YEAR, "y", expected);
            rule_plan(std::slice::from_ref(&document), &["c1"], &[rule])
        };
        let integer = |kind: DataKind, bits: u32| {
            move |field: &mut FieldT| {
                field.kind = kind;
                field.bit_length = bits;
            }
        };
        let bytes = |length: u32| integer(DataKind::Bytes, length * 8);
        let text = |kind: DataKind, length: u32| integer(kind, length * 8);
        let refused = |result: Result<CheckedPlan, PlanRefused>| match result {
            Err(PlanRefused::FieldSource { why, .. }) => why,
            other => panic!("not refused as a field source: {other:?}"),
        };
        // Numbers: one byte holds 255 but not 256; -128 but not 128; BCD two digits hold 99.
        assert!(outcome(&integer(DataKind::Uint, 8), &[0xFF]).is_ok());
        assert!(outcome(&integer(DataKind::Uint, 8), &[0, 0xFF]).is_ok());
        let why = refused(outcome(&integer(DataKind::Uint, 8), &[1, 0]));
        assert!(why.contains("width"), "{why}");
        assert!(outcome(&integer(DataKind::Int, 8), &[0x80]).is_ok());
        refused(outcome(&integer(DataKind::Int, 8), &[0, 0x80]));
        assert!(outcome(&integer(DataKind::Bcd, 8), &[99]).is_ok());
        refused(outcome(&integer(DataKind::Bcd, 8), &[100]));
        // Fixed-length bytes and text: the expected value must fit the field.
        assert!(outcome(&bytes(2), &[1, 2]).is_ok());
        let why = refused(outcome(&bytes(2), &[1, 2, 3]));
        assert!(why.contains("longer"), "{why}");
        assert!(outcome(&text(DataKind::Utf8, 2), b"ab").is_ok());
        refused(outcome(&text(DataKind::Utf8, 2), b"abc"));
        // An ASCII byte of 0x80 or more is two bytes of UTF-8, so n bytes can reach 2n.
        assert!(outcome(&text(DataKind::Ascii, 2), b"abcd").is_ok());
        refused(outcome(&text(DataKind::Ascii, 2), b"abcde"));
        // Text compares in UTF-8.
        let why = refused(outcome(&text(DataKind::Ascii, 5), &[0xFF]));
        assert!(why.contains("UTF-8"), "{why}");
        // Bytes may hold any byte.
        assert!(outcome(&bytes(5), &[0xFF]).is_ok());
    }

    /// ADR-403: sources of one component on one request that cannot decode to one value (a text
    /// and bytes, or one data kind at two fixed lengths) are refused, so a rule field that can
    /// never be established has no outcome classes; a text field of another encoding at the same
    /// length, and a variable-length one, are not refused.
    #[test]
    fn sources_of_one_rule_field_that_can_never_agree_are_refused() {
        let a = (0x7E0, 0x7E8);
        let rule_pair = |second: (Sha256, Vec<u8>)| {
            let documents = [sw_doc("v1", a), second];
            let rules: [CandidateFieldRule<'_>; 2] = [
                ("c1", 0, 1, 1, YEAR, "a", b"S"),
                ("c1", 1, 1, 1, YEAR, "a", b"S"),
            ];
            rule_plan(&documents, &["c1"], &rules)
        };
        let named_with = |name: &str, edit: fn(&mut FieldT)| with_field(&sw_doc(name, a), edit);
        let why = |result: Result<CheckedPlan, PlanRefused>| match result {
            Err(PlanRefused::FieldSource { why, .. }) => why,
            other => panic!("{other:?}"),
        };
        assert!(
            why(rule_pair(named_with("v2", |f| f.kind = DataKind::Bytes)))
                .contains("cannot decode to one value")
        );
        assert!(
            why(rule_pair(named_with("v2", |f| f.bit_length = 48)))
                .contains("cannot decode to one value")
        );
        assert!(rule_pair(named_with("v2", |f| f.kind = DataKind::Utf8)).is_ok());
        assert!(rule_pair(named_with("v2", |f| f.length_kind = LengthKind::Terminated)).is_ok());
        assert!(rule_pair(named_with("v2", |_| {})).is_ok());
    }

    /// ADR-403: two link groups of one definition at one endpoint that read one request for one
    /// component are two rule fields, told apart in the report and in the contradiction by the
    /// group's index.
    #[test]
    fn two_link_groups_reading_one_request_report_their_own_rule_fields() {
        let a = (0x7E0, 0x7E8);
        let block = |size: i64| {
            move |p: &mut ProtocolT| {
                p.functional_id = 0;
                with_param("CP_BlockSize", size)(p);
            }
        };
        let documents = [
            identified_by(0x89, 5, b"S", "v1", a, block(8)),
            identified_by(0x89, 5, b"S", "v2", a, block(16)),
        ];
        let rules: [CandidateFieldRule<'_>; 2] = [
            ("c1", 0, 1, 1, YEAR, "a", b"SW1"),
            ("c1", 1, 1, 1, YEAR, "a", b"SW1"),
        ];
        let plan = rule_plan(&documents, &["c1", "c2"], &rules).unwrap();
        let mut report = plan.report();
        let fields = report.rule_fields();
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].request, fields[1].request);
        assert_eq!((fields[0].group, fields[1].group), (0, 1));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["rule_fields"][1]["group"], 1);
        // The contradiction names the group whose field was established outside the values.
        for field in &mut report.rule_fields {
            field.established = Some(FieldValue::Text("x".into()));
            field.rules[0].outcome = RuleOutcome::Matched;
        }
        report.rule_fields[1].rules[0].outcome = RuleOutcome::NotMatched;
        let found = contradictions(&plan, &[], &[], &report.rule_fields);
        assert_eq!(found.len(), 1);
        assert!(matches!(
            found[0].1[..],
            [Contradiction::FieldRules { group: 1, .. }]
        ));
        let json = serde_json::to_value(&found[0].1).unwrap();
        assert_eq!(json[0]["FieldRules"]["group"], 1);
    }
}
