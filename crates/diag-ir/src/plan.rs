//! The identification plan an identification job carries (design 8.8.2, ADR-327, ADR-395).
//!
//! The server compiles a plan from the candidate vehicle definitions; the agent reads it with
//! the documents it names. The plan is a postcard struct with its own schema version, like
//! [`crate::Program`], and names every document by its SHA-256, so the signature that covers
//! the SHA-256 of the plan's bytes covers the documents too. It holds no ECU declarations
//! itself: a rule's source is a service and a field as one variant document declares them
//! (ADR-318 keeps those identifiers local to a document).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The plan format's version. It is the plan's own, not [`crate::IR_SCHEMA_VERSION`]: a plan
/// changes with the vehicle definitions, a document with the declaration schema (ADR-395).
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// The SHA-256 of a document's bytes.
pub type Sha256 = [u8; 32];

/// An identification component of a vehicle definition (ADR-326 item 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Component {
    Oem,
    VehicleModel,
    ModelYear,
    VehicleType,
}

/// Where a rule reads its value: a service and a field of one variant document (ADR-327 items
/// 4 and 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub document: Sha256,
    pub service: u32,
    pub field: u32,
}

/// A value of one component that the vehicle definition states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentificationValue {
    pub component: Component,
    pub value: String,
}

/// A VIN position range compared with expected text (ADR-326 item 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VinRule {
    pub component: Component,
    pub value: String,
    /// The first and last VIN position compared, from 1.
    pub start: u8,
    pub end: u8,
    pub expected: String,
    pub source: Source,
}

/// A field of a read service compared with an expected value as variant patterns compare
/// (ADR-306).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRule {
    pub component: Component,
    pub value: String,
    pub source: Source,
    pub expected: Vec<u8>,
}

/// An ECU entry of a vehicle definition: the ECU definitions that may fill the role, in order
/// (ADR-326 item 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub role: String,
    pub optional: bool,
    /// The role of the entry this one is reached through.
    pub gateway: Option<String>,
    /// Ids of [`IdentificationPlan::definitions`].
    pub alternatives: Vec<String>,
}

/// A vehicle definition the identification may conclude.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub values: Vec<IdentificationValue>,
    pub vin_rules: Vec<VinRule>,
    pub field_rules: Vec<FieldRule>,
    pub entries: Vec<Entry>,
}

/// An ECU definition the plan names, with the documents of its variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDefinition {
    /// The definition's identifier (`EcuDocument.definition_id`, ADR-363).
    pub id: String,
    pub documents: Vec<Sha256>,
}

/// How the vehicle was chosen, which the job reports (ADR-327 items 6 and 8). Only
/// `Candidates` narrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Selection {
    /// The server's candidates: every vehicle definition no established value contradicts.
    Candidates,
    /// One vehicle definition the operator chose by hand. The plan holds it as the only
    /// candidate, which supplies its entries and VIN rules and narrows nothing.
    ChosenDefinition,
    /// Access to every ECU the tenant may use: definitions without candidates.
    EveryEcu,
}

impl Selection {
    /// Whether facts drop candidates.
    pub fn narrows(self) -> bool {
        self == Self::Candidates
    }
}

/// The plan. `schema_version` stays the first field: [`IdentificationPlan::decode`] reads it
/// alone first, so a plan of another version is refused as such, not as a misparse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentificationPlan {
    pub schema_version: u32,
    pub selection: Selection,
    pub candidates: Vec<Candidate>,
    pub definitions: Vec<PlanDefinition>,
}

/// Why a plan cannot be read or is malformed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("the plan does not serialize or parse: {0}")]
    Format(postcard::Error),
    #[error("the plan is of schema version {found}; this build reads {expected}")]
    SchemaVersion { found: u32, expected: u32 },
    #[error("the plan has {0} bytes after its end")]
    TrailingBytes(usize),
    #[error("a plan for {selection:?} holds {found} candidates, not {expected}")]
    CandidateCount {
        selection: Selection,
        found: usize,
        expected: usize,
    },
    #[error("candidate id {0:?} is used twice")]
    DuplicateCandidate(String),
    #[error("definition id {0:?} is used twice")]
    DuplicateDefinition(String),
    #[error("candidate {candidate:?} names definition {definition:?}, which the plan lacks")]
    UnknownDefinition {
        candidate: String,
        definition: String,
    },
    #[error("candidate {candidate:?} reads document {document}, which no definition lists")]
    UnknownDocument { candidate: String, document: String },
}

/// Lowercase hexadecimal, as the logs and the errors write a hash.
pub fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl IdentificationPlan {
    /// The plan's bytes, whose SHA-256 the job's signature covers.
    pub fn encode(&self) -> Result<Vec<u8>, PlanError> {
        postcard::to_allocvec(self).map_err(PlanError::Format)
    }

    /// Reads a plan: the version first, then the whole plan with no bytes left over, then
    /// [`IdentificationPlan::validate`].
    pub fn decode(bytes: &[u8]) -> Result<Self, PlanError> {
        let (found, _) = postcard::take_from_bytes::<u32>(bytes).map_err(PlanError::Format)?;
        if found != PLAN_SCHEMA_VERSION {
            return Err(PlanError::SchemaVersion {
                found,
                expected: PLAN_SCHEMA_VERSION,
            });
        }
        let (plan, rest) = postcard::take_from_bytes::<Self>(bytes).map_err(PlanError::Format)?;
        if !rest.is_empty() {
            return Err(PlanError::TrailingBytes(rest.len()));
        }
        plan.validate()?;
        Ok(plan)
    }

    /// The structural checks that need no document: the candidate count each selection implies
    /// (item 8 of ADR-327), unique candidate and definition ids,
    /// entries that name definitions of the plan, and rule sources that name documents some
    /// definition lists.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(PlanError::SchemaVersion {
                found: self.schema_version,
                expected: PLAN_SCHEMA_VERSION,
            });
        }
        let expected = match self.selection {
            Selection::Candidates => self.candidates.len(),
            Selection::ChosenDefinition => 1,
            Selection::EveryEcu => 0,
        };
        if self.candidates.len() != expected {
            return Err(PlanError::CandidateCount {
                selection: self.selection,
                found: self.candidates.len(),
                expected,
            });
        }
        let mut definitions = HashSet::new();
        for definition in &self.definitions {
            if !definitions.insert(definition.id.as_str()) {
                return Err(PlanError::DuplicateDefinition(definition.id.clone()));
            }
        }
        let documents: HashSet<&Sha256> = self
            .definitions
            .iter()
            .flat_map(|definition| &definition.documents)
            .collect();
        let mut candidates = HashSet::new();
        for candidate in &self.candidates {
            if !candidates.insert(candidate.id.as_str()) {
                return Err(PlanError::DuplicateCandidate(candidate.id.clone()));
            }
            let named = candidate.entries.iter().flat_map(|e| &e.alternatives);
            for definition in named {
                if !definitions.contains(definition.as_str()) {
                    return Err(PlanError::UnknownDefinition {
                        candidate: candidate.id.clone(),
                        definition: definition.clone(),
                    });
                }
            }
            let sources = candidate
                .vin_rules
                .iter()
                .map(|rule| &rule.source)
                .chain(candidate.field_rules.iter().map(|rule| &rule.source));
            for source in sources {
                if !documents.contains(&source.document) {
                    return Err(PlanError::UnknownDocument {
                        candidate: candidate.id.clone(),
                        document: hex(&source.document),
                    });
                }
            }
        }
        Ok(())
    }

    /// Every document the plan names, once each, in the order the definitions list them.
    pub fn document_hashes(&self) -> Vec<Sha256> {
        let mut seen = HashSet::new();
        self.definitions
            .iter()
            .flat_map(|definition| definition.documents.iter().copied())
            .filter(|hash| seen.insert(*hash))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_A: Sha256 = [0xAA; 32];
    const DOC_B: Sha256 = [0xBB; 32];

    fn source(document: Sha256) -> Source {
        Source {
            document,
            service: 1,
            field: 1,
        }
    }

    fn candidate(id: &str, alternatives: &[&str], document: Sha256) -> Candidate {
        Candidate {
            id: id.to_owned(),
            values: vec![IdentificationValue {
                component: Component::VehicleModel,
                value: "m".to_owned(),
            }],
            vin_rules: vec![VinRule {
                component: Component::ModelYear,
                value: "2020".to_owned(),
                start: 10,
                end: 10,
                expected: "L".to_owned(),
                source: source(document),
            }],
            field_rules: vec![FieldRule {
                component: Component::Oem,
                value: "oem".to_owned(),
                source: source(document),
                expected: vec![1, 2],
            }],
            entries: vec![Entry {
                role: "engine".to_owned(),
                optional: false,
                gateway: None,
                alternatives: alternatives.iter().map(|&a| a.to_owned()).collect(),
            }],
        }
    }

    fn plan() -> IdentificationPlan {
        IdentificationPlan {
            schema_version: PLAN_SCHEMA_VERSION,
            selection: Selection::Candidates,
            candidates: vec![candidate("c1", &["ecu"], DOC_A)],
            definitions: vec![PlanDefinition {
                id: "ecu".to_owned(),
                documents: vec![DOC_A, DOC_B, DOC_A],
            }],
        }
    }

    #[test]
    fn a_plan_round_trips_and_names_each_document_once() {
        let plan = plan();
        let bytes = plan.encode().unwrap();
        assert_eq!(IdentificationPlan::decode(&bytes).unwrap(), plan);
        assert_eq!(plan.document_hashes(), vec![DOC_A, DOC_B]);
    }

    #[test]
    fn a_plan_of_another_version_is_refused_as_such() {
        let mut other = plan();
        other.schema_version = PLAN_SCHEMA_VERSION + 1;
        let bytes = other.encode().unwrap();
        assert_eq!(
            IdentificationPlan::decode(&bytes),
            Err(PlanError::SchemaVersion {
                found: PLAN_SCHEMA_VERSION + 1,
                expected: PLAN_SCHEMA_VERSION
            })
        );
        assert_eq!(
            other.validate(),
            Err(PlanError::SchemaVersion {
                found: PLAN_SCHEMA_VERSION + 1,
                expected: PLAN_SCHEMA_VERSION
            })
        );
    }

    #[test]
    fn a_truncated_plan_or_one_with_trailing_bytes_is_refused() {
        let bytes = plan().encode().unwrap();
        assert!(matches!(
            IdentificationPlan::decode(&bytes[..bytes.len() - 1]),
            Err(PlanError::Format(_))
        ));
        let mut longer = bytes;
        longer.push(0);
        assert_eq!(
            IdentificationPlan::decode(&longer),
            Err(PlanError::TrailingBytes(1))
        );
        assert!(matches!(
            IdentificationPlan::decode(&[]),
            Err(PlanError::Format(_))
        ));
    }

    #[test]
    fn each_selection_holds_the_candidates_it_implies() {
        let mut chosen = plan();
        chosen.selection = Selection::ChosenDefinition;
        assert_eq!(chosen.validate(), Ok(()));
        chosen.candidates.push(candidate("c2", &["ecu"], DOC_A));
        assert_eq!(
            chosen.validate(),
            Err(PlanError::CandidateCount {
                selection: Selection::ChosenDefinition,
                found: 2,
                expected: 1
            })
        );
        let mut every = plan();
        every.selection = Selection::EveryEcu;
        assert_eq!(
            every.validate(),
            Err(PlanError::CandidateCount {
                selection: Selection::EveryEcu,
                found: 1,
                expected: 0
            })
        );
        every.candidates.clear();
        assert_eq!(every.validate(), Ok(()));
        assert!(Selection::Candidates.narrows());
        assert!(!Selection::ChosenDefinition.narrows() && !Selection::EveryEcu.narrows());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let mut twice = plan();
        twice.candidates.push(candidate("c1", &["ecu"], DOC_A));
        assert_eq!(
            twice.validate(),
            Err(PlanError::DuplicateCandidate("c1".to_owned()))
        );
        let mut twice = plan();
        twice.definitions.push(twice.definitions[0].clone());
        assert_eq!(
            twice.validate(),
            Err(PlanError::DuplicateDefinition("ecu".to_owned()))
        );
    }

    #[test]
    fn an_entry_naming_an_unknown_definition_is_refused() {
        let mut unknown = plan();
        unknown.candidates[0].entries[0].alternatives = vec!["ecu".to_owned(), "gone".to_owned()];
        let bytes = unknown.encode().unwrap();
        assert_eq!(
            IdentificationPlan::decode(&bytes),
            Err(PlanError::UnknownDefinition {
                candidate: "c1".to_owned(),
                definition: "gone".to_owned()
            })
        );
    }

    #[test]
    fn a_source_naming_a_document_no_definition_lists_is_refused() {
        let mut unknown = plan();
        unknown.candidates[0].field_rules[0].source.document = [0xCC; 32];
        assert_eq!(
            unknown.validate(),
            Err(PlanError::UnknownDocument {
                candidate: "c1".to_owned(),
                document: "cc".repeat(32)
            })
        );
    }
}
