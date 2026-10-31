//! The identification job's plan: loading it with its documents, the checks before any link
//! opens, and the endpoints and presence groups derived from the declared links (design 8.8.2,
//! ADR-327, ADR-395).
//!
//! [`load_plan`] and [`check_plan`] send nothing and are the only gate before a link opens, and
//! [`CheckedPlan::report`] is the only writer of the report's endpoint and presence-group lists.
//! [`run_identification`] then sends the plan's functional presence requests and is the only
//! writer of the report's presence section (ADR-396); it sends nothing else yet.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diag_ir::declaration::EcuVariant;
use diag_ir::plan::{IdentificationPlan, PlanError, Selection, Sha256, hex};
use serde::{Serialize, Serializer};
use sha2::Digest;
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::client::{WorkerExit, WorkerSource, WorkerStatus};
use crate::comm_log::{CommLogSetup, CommLogStatus, JobLog, RunFacts, RunKind};
use crate::document::{DocumentError, JobDocument};
use crate::guards::JobGuards;
use crate::host::{
    HostError, OwnRead, ProcedurePosition, ReopenAttempt, ResendClass, Timings, names_lost_vci,
    settle_worker,
};
use crate::link::{BUS_TYPE_DWCAN, CloseEvidence, LinkConfig, MAX_CAN_ID};
use crate::policy::{ApplicationLayer, PRESENCE_REQUEST};
use crate::runner::{
    CancelOnDrop, GuardSlot, JobError, JobLimits, LinkRefused, WORKER_EXIT_GRACE, close_job_link,
    on_job_thread, open_job_link, reopen_attempts,
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
    /// Raises each timing of the group to `link`'s when that is larger: a larger value only
    /// makes the tester wait longer or send later (ADR-395 item 6).
    fn take_largest(&mut self, link: &LinkConfig) {
        self.p2_max_ms = self.p2_max_ms.max(link.p2_max_ms);
        self.p2_star_ms = self.p2_star_ms.max(link.p2_star_ms);
        self.rc78_completion_ms = self.rc78_completion_ms.max(link.rc78_completion_ms);
        for (name, value) in link.params.iter().filter(|(name, _)| is_timing(name)) {
            let name = name.to_ascii_lowercase();
            match self.timings.iter_mut().find(|(known, _)| *known == name) {
                Some((_, largest)) => *largest = (*largest).max(*value),
                None => self.timings.push((name, *value)),
            }
        }
        self.timings.sort();
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
    Ok(LoadedPlan { plan, documents })
}

/// A declared link, with the functional identifier its protocol declares (0 when none).
#[derive(Debug)]
struct DocumentLink {
    document: Sha256,
    link: LinkConfig,
    functional_id: u32,
}

/// A plan that passed [`check_plan`].
#[derive(Debug)]
pub struct CheckedPlan {
    selection: Selection,
    candidates: Vec<String>,
    links: Vec<DocumentLink>,
    refused_links: Vec<RefusedLink>,
}

/// The checks that run before any link opens: every service a pattern or a rule source names
/// is read-only and declared, every document's link is derived (a link ADR-324 refuses is
/// reported, not refused), and the links of one bus agree in their ComParams other than the
/// timings.
pub fn check_plan(loaded: &LoadedPlan) -> Result<CheckedPlan, PlanRefused> {
    let document = |hash: &Sha256| {
        let found = loaded.documents.iter().find(|(h, _)| h == hash);
        &found
            .expect("validate: every source names a listed document")
            .1
    };
    for (hash, document) in &loaded.documents {
        let variant = document.variant();
        for pattern in variant.patterns().into_iter().flatten() {
            check_read_only(hash, &variant, pattern.service_id())?;
        }
    }
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
    Ok(CheckedPlan {
        selection: loaded.plan.selection,
        candidates,
        links,
        refused_links,
    })
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

/// What the plan's checks and the derivation give (design 8.8.2), and, once
/// [`run_identification`] has run, the presence section: which response identifiers answered
/// the functional presence requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentificationReport {
    selection: Selection,
    candidates: Vec<String>,
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
    /// Whether the endpoint's bus and response identifier answered a presence request: an answer
    /// on a pair marks every endpoint with it, since alternatives often share one. Presence
    /// drops no candidate (ADR-327 item 2).
    pub fn answered(&self, endpoint: &EndpointKey) -> bool {
        self.presence
            .iter()
            .any(|answer| answer.bus == endpoint.bus && answer.rx_id == endpoint.rx_id)
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
        }
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
        link.p2_max_ms = group.p2_max_ms;
        link.p2_star_ms = group.p2_star_ms;
        link.rc78_completion_ms = group.rc78_completion_ms;
        for (name, value) in &group.timings {
            match link
                .params
                .iter_mut()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
            {
                Some((_, known)) => *known = *value,
                None => link.params.push((name.clone(), *value)),
            }
        }
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

/// Runs the plan's functional presence requests (ADR-327 item 2, ADR-396): per presence group, a
/// receive table of every declared pair on the group's bus, one link and one request at a time,
/// under the per-VCI lock `guards` hold. A table the VCI refuses (the worker's answer to the
/// table or the connect, never a timeout or a lost path) is halved in declaration order, each
/// part getting a link and a request of its own; a single pair still refused refuses the job.
/// When the path to the VCI is lost, the link is opened again within `limits` (ADR-393, ADR-394)
/// and the request sent again within `limits.max_primitive_repeats` with the origin `repeat`
/// (ADR-392); what was heard before the loss stays, and a run whose bounds run out reports
/// `presence_interrupted`. A cancel (`cancel`, also set when the future is dropped) ends the run
/// in [`JobError::Cancelled`]. `source` also gives the clients of the later links.
pub async fn run_identification<W: WorkerSource>(
    mut source: W,
    plan: CheckedPlan,
    limits: JobLimits,
    guards: JobGuards,
    comm_log: Option<CommLogSetup>,
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
    let log = JobLog::new(comm_log, RunFacts::default());
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
    Ok(report)
}

/// A table cut in two, the first half the larger; `None` for a single pair.
fn halves(table: &[(u32, u32)]) -> Option<[Vec<(u32, u32)>; 2]> {
    (table.len() > 1).then(|| {
        let (first, second) = table.split_at(table.len().div_ceil(2));
        [first.to_vec(), second.to_vec()]
    })
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
    let timings = Timings::for_link(config);
    let try_open = || {
        let stop = |error| LinkRefused {
            error,
            cleaned_up: true,
            table_refused: false,
            evidence: CloseEvidence::default(),
        };
        // A cancel, a worker that exited and the guards' mark are judged before each open: a
        // link the worker may still hold ends the job (ADR-258).
        if cancelled.load(Ordering::Relaxed) {
            return Err(stop(JobError::Cancelled));
        }
        let mut shared = SharedSource(Arc::clone(source));
        settle(
            &mut shared,
            CloseEvidence::default(),
            &comm_log,
            guards,
            worker_ended,
        );
        if link_unconfirmed(guards) {
            return Err(stop(JobError::LinkUnconfirmed));
        }
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
            (None, None) => return Ok(PartEnd::Interrupted(Vec::new(), false)),
        };
    }
    let link = match opened {
        Ok(link) => link,
        // A refusal whose teardown failed left the guards marked: nothing more opens (ADR-258).
        Err(refused) if refused.table_refused && refused.cleaned_up => {
            return Ok(PartEnd::Refused(refused.error));
        }
        Err(refused) => return Err(refused.error),
    };
    let crate::runner::JobLink {
        mut host,
        open_link,
        path_lost,
        ..
    } = link;
    let mut heard = Vec::new();
    let mut attempts = 0u8;
    let ended = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Err(JobError::Cancelled);
        }
        // The origin is the identification's, and `repeat` for a request sent again (ADR-392).
        host.own_reads(Some(OwnRead::Identification));
        if attempts > 0 {
            host.step_resends();
        }
        let presence = host.presence(&PRESENCE_REQUEST);
        host.step_ends();
        host.own_reads(None);
        heard.extend(presence.answers);
        let Err(error) = presence.end else {
            break Ok(PartEnd::Done(heard));
        };
        if error.resend_class() == ResendClass::PathLost {
            host.path_lost();
        }
        let again = attempts < limits.max_primitive_repeats;
        // A receive error that ended the window is not a clean window, and sending the request
        // again would not tell more: the part is interrupted with its answers (ADR-396).
        if matches!(error, HostError::PrimitiveFailed(_)) {
            break Ok(PartEnd::Interrupted(heard, true));
        }
        match error.resend_class() {
            // The ECU, the bus or the events failed the window, or the start was refused for a
            // reason that passes: the same link carries the request again.
            ResendClass::EcuLevel | ResendClass::Transient if again => {
                std::thread::sleep(limits.wait_poll.max(std::time::Duration::from_millis(1)));
            }
            ResendClass::PathLost if again => {
                match reopen_attempts(|| host.reopen_link(), limits, cancelled, reopens) {
                    Ok(true) => {}
                    Ok(false) => break Ok(PartEnd::Interrupted(heard, true)),
                    Err(error) => break Err(error),
                }
            }
            ResendClass::EcuLevel | ResendClass::PathLost | ResendClass::Transient => {
                break Ok(PartEnd::Interrupted(heard, true));
            }
            ResendClass::Other => break Err(JobError::Link(error)),
        }
        attempts += 1;
    };
    close_job_link(host, open_link, &path_lost);
    ended
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
        ComParamT, DataKind, EcuDocumentT, EcuVariantT, ProtocolT, ServiceT, VariantPatternT,
        encode_ecu_document,
    };
    use diag_ir::plan::{
        Candidate, Component, Entry, PLAN_SCHEMA_VERSION, PlanDefinition, Selection as Chosen,
        Source, VinRule,
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
            4,
        )]);
        let mut document = EcuDocumentT::default();
        document.schema_version = IR_SCHEMA_VERSION;
        document.source_format = Some("csv-starlark".into());
        document.source_digest = Some("00".into());
        document.variant = Some(Box::new(variant));
        encode_ecu_document(&document)
    }

    /// `bytes` with its variant changed by `edit`.
    fn edited(bytes: &[u8], edit: impl FnOnce(&mut EcuVariantT)) -> Vec<u8> {
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
                vin_rules: vec![VinRule {
                    component: Component::ModelYear,
                    value: "2020".into(),
                    start: 10,
                    end: 10,
                    expected: "L".into(),
                    source,
                }],
                field_rules: Vec::new(),
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

    /// Loads and checks a plan over `documents`.
    pub(super) fn checked(documents: &[(Sha256, Vec<u8>)]) -> Result<CheckedPlan, PlanRefused> {
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
        bad.candidates[0].vin_rules[0].source.field = 9;
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
}
