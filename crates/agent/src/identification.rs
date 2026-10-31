//! The identification job's plan: loading it with its documents, the checks before any link
//! opens, and the endpoints and presence groups derived from the declared links (design 8.8.2,
//! ADR-327, ADR-395).
//!
//! This module sends nothing. [`load_plan`] and [`check_plan`] are the only gate before a link
//! opens, and [`CheckedPlan::report`] is the only writer of the report's endpoint and
//! presence-group lists.

use diag_ir::declaration::EcuVariant;
use diag_ir::plan::{IdentificationPlan, PlanError, Selection, Sha256, hex};
use sha2::Digest;

use crate::document::{DocumentError, JobDocument};
use crate::link::{BUS_TYPE_DWCAN, LinkConfig, MAX_CAN_ID};
use crate::policy::ApplicationLayer;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointKey {
    pub bus: BusKey,
    pub tx_id: u32,
    pub rx_id: u32,
}

/// An endpoint and the documents whose declared link is on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointEntry {
    pub key: EndpointKey,
    pub documents: Vec<Sha256>,
}

/// The links that send one functional presence request together: a bus and a functional
/// identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceGroup {
    pub bus: BusKey,
    pub functional_id: u32,
    /// `CP_P2Max` of the presence link, in milliseconds: the longest over the declared links of
    /// the group (ADR-395 item 5). Not used for traffic yet.
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

/// A presence answer: the bus and the response identifier that answered (ADR-327 item 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceAnswer {
    pub bus: BusKey,
    pub rx_id: u32,
}

/// A document whose declared link ADR-324 refuses: the job cannot read it, which leaves its
/// definition inconclusive at read time (ADR-327 item 3), and the plan stays valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedLink {
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

/// What the plan's checks and the derivation give (design 8.8.2). The presence section stays
/// empty until the presence request is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentificationReport {
    selection: Selection,
    candidates: Vec<String>,
    endpoints: Vec<EndpointEntry>,
    presence_groups: Vec<PresenceGroup>,
    buses_without_presence: Vec<BusKey>,
    refused_links: Vec<RefusedLink>,
    presence: Vec<PresenceAnswer>,
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
        }
    }
}

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
    fn document(did: u8, tx: u32, rx: u32, setup: impl FnOnce(&mut ProtocolT)) -> Vec<u8> {
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

    fn named(bytes: Vec<u8>) -> (Sha256, Vec<u8>) {
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
    fn checked(documents: &[(Sha256, Vec<u8>)]) -> Result<CheckedPlan, PlanRefused> {
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
