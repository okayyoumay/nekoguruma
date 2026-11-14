//! The messages of the control channel between the agent and the server.
//!
//! Design document sections: 5.2 communication channels / 5.3 handling disconnections /
//! 14.1 keeping WebSockets alive. ADR-466 fixes the channel; `api/asyncapi.yaml` (`agentControl`)
//! describes it message by message, and the tests here parse its examples.
//!
//! Every message is one JSON object in one text frame. Its `type` is the asyncapi message name
//! and the other properties are camelCase. No message carries an agent ID: the server takes it
//! from the session that opened the connection (ADR-466). These types are the wire form only;
//! [`AgentEvent`](crate::AgentEvent) stays the in-process event type.

use serde::{Deserialize, Serialize};

use crate::{InstructionRejectReason, JobId};

/// The WebSocket subprotocol of this version of the channel. An upgrade that does not offer it
/// is refused (ADR-466).
pub const CONTROL_SUBPROTOCOL: &str = "ngr-control.v1";

/// The longest message, in bytes of its JSON text, in either direction (ADR-466). A full job
/// command (a 64 KiB envelope in base64) is about 87.6 KiB.
pub const MAX_CONTROL_MESSAGE_LEN: usize = 128 * 1024;

/// The heartbeat interval the server names in `hello` unless configured otherwise (14.1).
pub const DEFAULT_HEARTBEAT_SECS: u32 = 20;

/// The shortest and the longest heartbeat interval `hello` may name. The longest is the idle
/// timeout commonly met at a load balancer or proxy (60 seconds): a longer interval could not
/// keep such a connection open (14.1).
pub const MIN_HEARTBEAT_SECS: u32 = 1;
pub const MAX_HEARTBEAT_SECS: u32 = 60;

/// The largest value of `progressPermille`.
pub const MAX_PROGRESS_PERMILLE: u16 = 1000;

/// How many intervals without any frame a side waits before it closes the connection (14.1).
pub const HEARTBEAT_MISSED_LIMIT: u32 = 3;

/// The standard close codes the two sides send (RFC 6455 section 7.4.1): the sender is stopping,
/// a protocol violation, a frame of a kind the channel does not take, and a message over the
/// limit (ADR-466, ADR-471).
pub const CLOSE_CODE_GOING_AWAY: u16 = 1001;
pub const CLOSE_CODE_PROTOCOL: u16 = 1002;
pub const CLOSE_CODE_BINARY: u16 = 1003;
pub const CLOSE_CODE_TOO_BIG: u16 = 1009;

/// The application close code with which the server ends a connection that a newer connection
/// of the same agent replaced (ADR-466).
pub const CLOSE_CODE_REPLACED: u16 = 4001;

/// The application close code with which the server ends a connection of an agent whose key was
/// revoked, or whose session ended (ADR-466 item 5, ADR-468).
pub const CLOSE_CODE_REVOKED: u16 = 4002;

/// The application close code with which either side ends a connection that has seen no frame of
/// any kind for [`HEARTBEAT_MISSED_LIMIT`] heartbeat intervals (14.1).
pub const CLOSE_CODE_IDLE: u16 = 4003;

/// Every close code the server itself sends, each with its own reason word in [`close_reason`].
pub const CLOSE_CODES: [u16; 7] = [
    CLOSE_CODE_GOING_AWAY,
    CLOSE_CODE_PROTOCOL,
    CLOSE_CODE_BINARY,
    CLOSE_CODE_TOO_BIG,
    CLOSE_CODE_REPLACED,
    CLOSE_CODE_REVOKED,
    CLOSE_CODE_IDLE,
];

/// The reason text a Close frame carries for `code`: one fixed word per code, never anything the
/// peer sent. Both sides use this table, so a reason says the same thing in either direction. A
/// code the table does not know gets `closed`.
pub fn close_reason(code: u16) -> &'static str {
    match code {
        CLOSE_CODE_GOING_AWAY => "stopping",
        CLOSE_CODE_PROTOCOL => "protocol",
        CLOSE_CODE_BINARY => "binary",
        CLOSE_CODE_TOO_BIG => "too big",
        CLOSE_CODE_REPLACED => "replaced",
        CLOSE_CODE_REVOKED => "revoked",
        CLOSE_CODE_IDLE => "idle",
        _ => "closed",
    }
}

/// A message could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The text is longer than [`MAX_CONTROL_MESSAGE_LEN`].
    #[error("a control message is at most {MAX_CONTROL_MESSAGE_LEN} bytes")]
    TooLarge,
    /// The text is not a message of this version: not JSON, no known `type` for the direction,
    /// a property of the wrong form, a missing required property or a property the message
    /// does not have. The connection is closed.
    #[error("not a control message of this version")]
    Malformed(#[source] serde_json::Error),
    /// The text is not a message of this version, for a reason that has no parser error: an
    /// unknown property or an unknown `type`. The connection is closed.
    #[error("not a control message of this version: {0}")]
    Refused(&'static str),
    /// A number is outside the range its property allows.
    #[error("{0} is out of range")]
    OutOfRange(&'static str),
}

impl ControlError {
    /// The close code with which a side ends a connection whose text did not decode: 1009 for a
    /// message over the limit, 1002 for anything else (ADR-466, ADR-471).
    pub fn close_code(&self) -> u16 {
        match self {
            ControlError::TooLarge => CLOSE_CODE_TOO_BIG,
            ControlError::Malformed(_) | ControlError::Refused(_) | ControlError::OutOfRange(_) => {
                CLOSE_CODE_PROTOCOL
            }
        }
    }
}

/// A message the server sends to the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    /// First frame of every connection: the interval at which the agent sends `heartbeat`.
    Hello {
        heartbeat_secs: u32,
    },
    /// A signed job instruction (5.3, ADR-461). `signed_job` is the base64 of one envelope.
    JobCommand {
        command_id: String,
        /// `None` when the property is missing or is not a string. The agent's gate then
        /// answers `commandRejected` with the reason `malformed`, so the command is settled
        /// (ADR-466); a message without `commandId` cannot be, and closes the connection.
        #[serde(
            default,
            deserialize_with = "lenient_string",
            skip_serializing_if = "Option::is_none"
        )]
        signed_job: Option<String>,
    },
    CancelCommand {
        job_id: JobId,
    },
    HmiResponse {
        job_id: JobId,
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        values: Option<serde_json::Map<String, serde_json::Value>>,
    },
    SeedKeyResponse {
        job_id: JobId,
        request_id: String,
        /// base64.
        key: String,
    },
    /// Every event up to and including `seq` is stored (5.3, ADR-478).
    EventAck {
        seq: u64,
    },
    /// The echo of the agent's heartbeat.
    Heartbeat,
}

fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    })
}

/// A message the agent sends to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentMessage {
    /// The job command was accepted; settles its delivery.
    CommandAck {
        command_id: String,
    },
    /// The job command was refused; settles its delivery (5.3, ADR-461, ADR-466).
    CommandRejected {
        command_id: String,
        reason: InstructionRejectReason,
    },
    AgentEvent(AgentEventMessage),
    Heartbeat,
    Reconcile {
        open_jobs: Vec<OpenJob>,
    },
}

/// An event, with its place in the agent's outbox when it is one that must be delivered (5.3,
/// ADR-478). The properties of the [`AgentEventKind`] sit beside `seq` in the same object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEventMessage {
    /// The event's number in the agent's outbox: at least 1 and contiguous per agent ID. Present
    /// on `jobState` and `hmiRequest`, absent on `progress`, which is never stored or resent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Device time, for reference only (16.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_agent: Option<String>,
    #[serde(flatten)]
    pub event: AgentEventKind,
}

/// What happened, selected by the object's `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentEventKind {
    JobState {
        job_id: JobId,
        /// The state's name from 5.6 in camelCase.
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure_code: Option<String>,
        /// The procedure's own code, with failure code `procedure` (5.6, ADR-475).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        procedure_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Progress {
        job_id: JobId,
        progress_permille: u16,
        phase: String,
    },
    HmiRequest {
        job_id: JobId,
        hmi_request: HmiRequestBody,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HmiRequestBody {
    pub request_id: String,
    /// Record template or HMI form.
    pub template_id: String,
}

/// One open job in a reconciliation (5.3); only open jobs are sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenJob {
    pub job_id: JobId,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
}

/// Summary for handover to another device (8.2.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
}

/// The properties each message of each direction may have, besides `type`. Serde cannot refuse
/// unknown properties on an internally tagged enum or a flattened one, so the check is made on
/// the parsed object before the typed read (ADR-466 item 8).
fn allowed(direction: Direction, ty: &str) -> Option<&'static [&'static str]> {
    use Direction::{ToAgent, ToServer};
    Some(match (direction, ty) {
        (ToAgent, "hello") => &["heartbeatSecs"],
        (ToAgent, "jobCommand") => &["commandId", "signedJob"],
        (ToAgent, "cancelCommand") => &["jobId"],
        (ToAgent, "hmiResponse") => &["jobId", "requestId", "values"],
        (ToAgent, "seedKeyResponse") => &["jobId", "requestId", "key"],
        (ToAgent, "eventAck") => &["seq"],
        (ToServer, "commandAck") => &["commandId"],
        (ToServer, "commandRejected") => &["commandId", "reason"],
        (ToServer, "reconcile") => &["openJobs"],
        (_, "heartbeat") => &[],
        // The properties of `agentEvent` depend on its `kind`; see `check_keys`.
        (ToServer, "agentEvent") => &["seq", "occurredAtAgent", "kind"],
        _ => return None,
    })
}

const EVENT_KIND_KEYS: [(&str, &[&str]); 3] = [
    (
        "jobState",
        &["jobId", "state", "failureCode", "procedureCode", "detail"],
    ),
    ("progress", &["jobId", "progressPermille", "phase"]),
    ("hmiRequest", &["jobId", "hmiRequest"]),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    ToAgent,
    ToServer,
}

fn check_keys(direction: Direction, text: &str) -> Result<serde_json::Value, ControlError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(ControlError::Malformed)?;
    let object = value
        .as_object()
        .ok_or(ControlError::Refused("not an object"))?;
    let ty = object
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or(ControlError::Refused("no type"))?;
    let keys = allowed(direction, ty).ok_or(ControlError::Refused("unknown type"))?;
    let extra: &[&str] = if ty == "agentEvent" {
        match object.get("kind").and_then(|k| k.as_str()) {
            Some(kind) => EVENT_KIND_KEYS
                .iter()
                .find(|(name, _)| *name == kind)
                .map(|(_, keys)| *keys)
                .ok_or(ControlError::Refused("unknown kind"))?,
            None => &[],
        }
    } else {
        &[]
    };
    for key in object.keys() {
        if key != "type" && !keys.contains(&key.as_str()) && !extra.contains(&key.as_str()) {
            return Err(ControlError::Refused("unknown property"));
        }
    }
    Ok(value)
}

fn decode<T: serde::de::DeserializeOwned>(
    direction: Direction,
    text: &str,
) -> Result<T, ControlError> {
    if text.len() > MAX_CONTROL_MESSAGE_LEN {
        return Err(ControlError::TooLarge);
    }
    let value = check_keys(direction, text)?;
    serde_json::from_value(value).map_err(ControlError::Malformed)
}

fn encode<T: Serialize>(message: &T) -> Result<String, ControlError> {
    let text = serde_json::to_string(message).map_err(ControlError::Malformed)?;
    if text.len() > MAX_CONTROL_MESSAGE_LEN {
        return Err(ControlError::TooLarge);
    }
    Ok(text)
}

fn check_secs(secs: u32) -> Result<(), ControlError> {
    if (MIN_HEARTBEAT_SECS..=MAX_HEARTBEAT_SECS).contains(&secs) {
        Ok(())
    } else {
        Err(ControlError::OutOfRange("heartbeatSecs"))
    }
}

fn check_permille(permille: u16) -> Result<(), ControlError> {
    if permille <= MAX_PROGRESS_PERMILLE {
        Ok(())
    } else {
        Err(ControlError::OutOfRange("progressPermille"))
    }
}

impl ServerMessage {
    /// Reads one text frame. Any error closes the connection.
    pub fn decode(text: &str) -> Result<Self, ControlError> {
        let message: Self = decode(Direction::ToAgent, text)?;
        message.check()?;
        Ok(message)
    }

    /// Writes one text frame, refusing one over the limit or out of range.
    pub fn encode(&self) -> Result<String, ControlError> {
        self.check()?;
        encode(self)
    }

    fn check(&self) -> Result<(), ControlError> {
        match self {
            Self::Hello { heartbeat_secs } => check_secs(*heartbeat_secs),
            _ => Ok(()),
        }
    }
}

impl AgentMessage {
    /// Reads one text frame. Any error closes the connection.
    pub fn decode(text: &str) -> Result<Self, ControlError> {
        let message: Self = decode(Direction::ToServer, text)?;
        message.check()?;
        Ok(message)
    }

    /// Writes one text frame, refusing one over the limit or out of range.
    pub fn encode(&self) -> Result<String, ControlError> {
        self.check()?;
        encode(self)
    }

    fn check(&self) -> Result<(), ControlError> {
        match self {
            Self::AgentEvent(AgentEventMessage {
                seq,
                event:
                    AgentEventKind::Progress {
                        progress_permille, ..
                    },
                ..
            }) => {
                // A progress event is never stored, so it has no place in the outbox.
                if seq.is_some() {
                    return Err(ControlError::Refused("progress has no seq"));
                }
                check_permille(*progress_permille)
            }
            Self::AgentEvent(AgentEventMessage { seq, .. }) => match seq {
                Some(0) => Err(ControlError::OutOfRange("seq")),
                Some(_) => Ok(()),
                None => Err(ControlError::Refused("no seq")),
            },
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// The examples of `api/asyncapi.yaml`, as JSON. The examples there are written as one-line
    /// flow mappings with quoted keys, which are both YAML and JSON, so a line that starts with
    /// `- payload: ` is one example and the rest of it is parsed as JSON. (The workspace has no
    /// YAML parser, and a test is not worth a dependency.)
    fn asyncapi_examples() -> Vec<Value> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/asyncapi.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        let examples: Vec<Value> = text
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("- payload: "))
            .map(|json| serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}")))
            .collect();
        // An example written in another form would be skipped above and drift unnoticed.
        assert_eq!(
            text.matches("- payload").count(),
            examples.len(),
            "every example must be one line, `- payload: {{...}}`, in JSON"
        );
        examples
    }

    fn type_of(v: &Value) -> &str {
        v["type"].as_str().unwrap()
    }

    const SERVER_TYPES: [&str; 7] = [
        "hello",
        "jobCommand",
        "cancelCommand",
        "hmiResponse",
        "seedKeyResponse",
        "eventAck",
        "heartbeat",
    ];
    const AGENT_TYPES: [&str; 5] = [
        "commandAck",
        "commandRejected",
        "agentEvent",
        "heartbeat",
        "reconcile",
    ];

    #[test]
    fn every_asyncapi_example_round_trips() {
        let examples = asyncapi_examples();
        for example in &examples {
            let text = example.to_string();
            let t = type_of(example);
            let mut known = false;
            if SERVER_TYPES.contains(&t) {
                known = true;
                let m = ServerMessage::decode(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
                let back: Value = serde_json::from_str(&m.encode().unwrap()).unwrap();
                assert_eq!(&back, example);
            }
            if AGENT_TYPES.contains(&t) {
                known = true;
                let m = AgentMessage::decode(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
                let back: Value = serde_json::from_str(&m.encode().unwrap()).unwrap();
                assert_eq!(&back, example);
            }
            assert!(known, "{t} is in no direction");
        }
    }

    #[test]
    fn the_examples_cover_every_message_name() {
        let examples = asyncapi_examples();
        let types: Vec<&str> = examples.iter().map(type_of).collect();
        for name in SERVER_TYPES.iter().chain(AGENT_TYPES.iter()) {
            assert!(types.contains(name), "no example for {name}");
        }
        // The three kinds of agent event that have no free-form part.
        let kinds: Vec<&str> = examples.iter().filter_map(|e| e["kind"].as_str()).collect();
        for kind in ["jobState", "progress", "hmiRequest"] {
            assert!(kinds.contains(&kind), "no example for kind {kind}");
        }
    }

    #[test]
    fn the_asyncapi_file_states_the_constants() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../api/asyncapi.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("address: /api/v1/agent/control"));
        assert!(text.contains(CONTROL_SUBPROTOCOL));
        assert!(text.contains("128 KiB"));
        assert_eq!(MAX_CONTROL_MESSAGE_LEN, 128 * 1024);
        for code in [CLOSE_CODE_REPLACED, CLOSE_CODE_REVOKED, CLOSE_CODE_IDLE] {
            assert!(text.contains(&format!("| {code} |")), "{code}");
        }
        assert!(text.contains(&format!("{DEFAULT_HEARTBEAT_SECS} by default")));
    }

    #[test]
    fn the_servers_own_failure_codes_have_their_wire_names() {
        use crate::FailureCode;
        assert_eq!(
            FailureCode::InstructionRejected.wire_name(),
            "instructionRejected"
        );
        assert_eq!(
            FailureCode::InstructionKeyRetired.wire_name(),
            "instructionKeyRetired"
        );
    }

    #[test]
    fn a_failed_job_state_carries_the_procedures_code_apart_from_the_agents() {
        use crate::{FailureCode, JobId, JobState};
        let state = JobState::Failed {
            code: FailureCode::Procedure,
            detail: "why".to_owned(),
            procedure_code: Some("flash.verify".to_owned()),
        };
        let event = AgentMessage::AgentEvent(AgentEventMessage {
            seq: Some(3),
            occurred_at_agent: None,
            event: state.to_event_kind(JobId("j-1".to_owned())),
        });
        let text = event.encode().unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["state"], "failed");
        assert_eq!(value["failureCode"], "procedure");
        assert_eq!(value["procedureCode"], "flash.verify");
        assert_eq!(value["detail"], "why");
        assert!(AgentMessage::decode(&text).is_ok());
        // A refusal carries its detail, and no procedure code.
        let kind = JobState::PreconditionFailed {
            code: FailureCode::JobRefused,
            detail: "d".to_owned(),
        }
        .to_event_kind(JobId("j".to_owned()));
        let AgentEventKind::JobState {
            state,
            failure_code,
            procedure_code,
            detail,
            ..
        } = kind
        else {
            panic!("not a job state");
        };
        assert_eq!(state, "preconditionFailed");
        assert_eq!(failure_code.as_deref(), Some("jobRefused"));
        assert_eq!(procedure_code, None);
        assert_eq!(detail.as_deref(), Some("d"));
        // An empty detail is left out of the event.
        let kind = JobState::NeedsOnSiteRecovery {
            detail: String::new(),
        }
        .to_event_kind(JobId("j".to_owned()));
        let AgentEventKind::JobState { detail, .. } = kind else {
            panic!("not a job state");
        };
        assert_eq!(detail, None);
    }

    #[test]
    fn the_heartbeat_has_no_payload() {
        assert_eq!(
            ServerMessage::Heartbeat.encode().unwrap(),
            r#"{"type":"heartbeat"}"#
        );
        assert_eq!(
            AgentMessage::Heartbeat.encode().unwrap(),
            r#"{"type":"heartbeat"}"#
        );
        assert!(matches!(
            AgentMessage::decode(r#"{"type":"heartbeat"}"#),
            Ok(AgentMessage::Heartbeat)
        ));
    }

    #[test]
    fn an_unknown_type_or_shape_is_refused() {
        for text in [
            r#"{"type":"nope"}"#,
            r#"{"heartbeatSecs":20}"#,
            r#"{"type":"hello"}"#,
            r#"{"type":"hello","heartbeatSecs":"20"}"#,
            r#"["hello"]"#,
            "not json",
            "",
        ] {
            assert!(ServerMessage::decode(text).is_err(), "{text}");
        }
        // A server message is not an agent message, and the reverse.
        assert!(AgentMessage::decode(r#"{"type":"hello","heartbeatSecs":20}"#).is_err());
        assert!(ServerMessage::decode(r#"{"type":"commandAck","commandId":"c"}"#).is_err());
        // An agent event of an unknown kind.
        assert!(AgentMessage::decode(r#"{"type":"agentEvent","seq":1,"kind":"capture"}"#).is_err());
    }

    #[test]
    fn unknown_properties_are_refused() {
        // Top level, for messages of both directions, and an agentId that no payload has.
        for text in [
            r#"{"type":"heartbeat","agentId":"other"}"#,
            r#"{"type":"heartbeat","sentAt":"2026-01-01T00:00:00Z"}"#,
            r#"{"type":"hello","heartbeatSecs":20,"extra":1}"#,
            r#"{"type":"jobCommand","commandId":"c","signedJob":"AA==","extra":true}"#,
            r#"{"type":"eventAck","seq":1,"agentId":"a"}"#,
        ] {
            assert!(
                matches!(ServerMessage::decode(text), Err(ControlError::Refused(_))),
                "{text}"
            );
        }
        for text in [
            r#"{"type":"commandAck","commandId":"c","extra":1}"#,
            r#"{"type":"commandRejected","commandId":"c","reason":"malformed","detail":"x"}"#,
            r#"{"type":"reconcile","openJobs":[],"agentId":"a"}"#,
            // A property of another kind of event.
            r#"{"type":"agentEvent","seq":1,"kind":"jobState","jobId":"j","state":"running","progressPermille":5}"#,
            r#"{"type":"agentEvent","kind":"progress","jobId":"j","progressPermille":5,"phase":"p","state":"x"}"#,
            r#"{"type":"agentEvent","seq":1,"kind":"jobState","jobId":"j","state":"running","agentId":"a"}"#,
        ] {
            assert!(
                matches!(AgentMessage::decode(text), Err(ControlError::Refused(_))),
                "{text}"
            );
        }
        // Nested payloads.
        for text in [
            r#"{"type":"reconcile","openJobs":[{"jobId":"j","state":"running","extra":1}]}"#,
            r#"{"type":"reconcile","openJobs":[{"jobId":"j","state":"running","checkpoint":{"section":1,"extra":1}}]}"#,
            r#"{"type":"agentEvent","seq":1,"kind":"hmiRequest","jobId":"j","hmiRequest":{"requestId":"r","templateId":"t","extra":1}}"#,
        ] {
            assert!(
                matches!(AgentMessage::decode(text), Err(ControlError::Malformed(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn a_guaranteed_event_has_a_seq_of_at_least_one_and_a_progress_event_has_none() {
        let state = |seq: &str| {
            format!(
                r#"{{"type":"agentEvent"{seq},"kind":"jobState","jobId":"j","state":"running"}}"#
            )
        };
        let hmi = r#"{"type":"agentEvent","seq":2,"kind":"hmiRequest","jobId":"j","hmiRequest":{"requestId":"r","templateId":"t"}}"#;
        let progress = |seq: &str| {
            format!(
                r#"{{"type":"agentEvent"{seq},"kind":"progress","jobId":"j","progressPermille":5,"phase":"x"}}"#
            )
        };
        assert!(AgentMessage::decode(&state(r#","seq":1"#)).is_ok());
        assert!(AgentMessage::decode(hmi).is_ok());
        assert!(AgentMessage::decode(&progress("")).is_ok());
        // A guaranteed event without a number, or with 0, and a progress event with one.
        assert!(matches!(
            AgentMessage::decode(&state("")),
            Err(ControlError::Refused(_))
        ));
        assert!(matches!(
            AgentMessage::decode(&state(r#","seq":0"#)),
            Err(ControlError::OutOfRange("seq"))
        ));
        assert!(matches!(
            AgentMessage::decode(&progress(r#","seq":1"#)),
            Err(ControlError::Refused(_))
        ));
        assert!(AgentMessage::decode(&state(r#","seq":-1"#)).is_err());
        // The old delivery property is gone.
        assert!(matches!(
            AgentMessage::decode(&state(r#","seq":1,"delivery":"guaranteed""#)),
            Err(ControlError::Refused(_))
        ));
        // The write side holds the same rule.
        let AgentMessage::AgentEvent(mut event) =
            AgentMessage::decode(&state(r#","seq":1"#)).unwrap()
        else {
            panic!("not an event");
        };
        event.seq = None;
        assert!(AgentMessage::AgentEvent(event).encode().is_err());
    }

    #[test]
    fn capture_log_and_capabilities_are_not_event_kinds() {
        for kind in ["capture", "log", "capabilities"] {
            let text = format!(r#"{{"type":"agentEvent","seq":1,"kind":"{kind}"}}"#);
            assert!(AgentMessage::decode(&text).is_err(), "{kind}");
        }
    }

    #[test]
    fn a_job_command_without_an_envelope_is_still_a_message() {
        // It reaches the gate, which answers commandRejected with the reason malformed.
        for text in [
            r#"{"type":"jobCommand","commandId":"c"}"#,
            r#"{"type":"jobCommand","commandId":"c","signedJob":7}"#,
            r#"{"type":"jobCommand","commandId":"c","signedJob":null}"#,
        ] {
            match ServerMessage::decode(text).unwrap() {
                ServerMessage::JobCommand {
                    command_id,
                    signed_job,
                } => {
                    assert_eq!(command_id, "c");
                    assert!(signed_job.is_none());
                }
                other => panic!("{other:?}"),
            }
        }
        // Without a command ID there is nothing to settle: the connection closes.
        for text in [
            r#"{"type":"jobCommand","signedJob":"AA=="}"#,
            r#"{"type":"jobCommand","commandId":5,"signedJob":"AA=="}"#,
            r#"{"type":"jobCommand"}"#,
        ] {
            assert!(
                matches!(ServerMessage::decode(text), Err(ControlError::Malformed(_))),
                "{text}"
            );
        }
        // An unknown property still closes, even with a missing envelope.
        assert!(ServerMessage::decode(r#"{"type":"jobCommand","commandId":"c","x":1}"#).is_err());
    }

    #[test]
    fn the_heartbeat_interval_is_bounded() {
        let hello = |secs: u32| format!(r#"{{"type":"hello","heartbeatSecs":{secs}}}"#);
        for ok in [
            MIN_HEARTBEAT_SECS,
            DEFAULT_HEARTBEAT_SECS,
            MAX_HEARTBEAT_SECS,
        ] {
            assert!(ServerMessage::decode(&hello(ok)).is_ok(), "{ok}");
            assert!(ServerMessage::Hello { heartbeat_secs: ok }.encode().is_ok());
        }
        for bad in [0, MAX_HEARTBEAT_SECS + 1, u32::MAX] {
            assert!(
                matches!(
                    ServerMessage::decode(&hello(bad)),
                    Err(ControlError::OutOfRange(_))
                ),
                "{bad}"
            );
            assert!(
                ServerMessage::Hello {
                    heartbeat_secs: bad
                }
                .encode()
                .is_err()
            );
        }
        assert!(ServerMessage::decode(r#"{"type":"hello","heartbeatSecs":-1}"#).is_err());
    }

    #[test]
    fn progress_is_at_most_a_thousand_permille() {
        let progress = |p: u32| {
            format!(
                r#"{{"type":"agentEvent","kind":"progress","jobId":"j","progressPermille":{p},"phase":"x"}}"#
            )
        };
        for ok in [0, 1, 1000] {
            let m = AgentMessage::decode(&progress(ok)).unwrap();
            assert!(m.encode().is_ok());
        }
        for bad in [1001, 65_535] {
            assert!(
                matches!(
                    AgentMessage::decode(&progress(bad)),
                    Err(ControlError::OutOfRange(_))
                ),
                "{bad}"
            );
        }
        assert!(AgentMessage::decode(&progress(65_536)).is_err());
        assert!(AgentMessage::decode(&progress(0).replace(":0,", ":-1,")).is_err());
    }

    #[test]
    fn the_size_limit_holds_both_ways() {
        let big = "x".repeat(MAX_CONTROL_MESSAGE_LEN);
        let text = json!({"type": "jobCommand", "commandId": "c", "signedJob": big}).to_string();
        assert!(text.len() > MAX_CONTROL_MESSAGE_LEN);
        assert!(matches!(
            ServerMessage::decode(&text),
            Err(ControlError::TooLarge)
        ));
        let m = ServerMessage::JobCommand {
            command_id: "c".into(),
            signed_job: Some(big),
        };
        assert!(matches!(m.encode(), Err(ControlError::TooLarge)));
        // A full envelope in base64 fits: 65,633 bytes are 87,512 characters (85.5 KiB).
        let full = ServerMessage::JobCommand {
            command_id: "c".into(),
            signed_job: Some("A".repeat(87_512)),
        };
        assert!(full.encode().is_ok());
    }

    #[test]
    fn every_close_code_has_its_own_reason_word() {
        let codes = CLOSE_CODES;
        let words: Vec<&str> = codes.iter().map(|code| close_reason(*code)).collect();
        let mut unique = words.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "{words:?}");
        assert!(!words.contains(&"closed"));
        assert_eq!(close_reason(1000), "closed");
    }

    #[test]
    fn a_message_that_does_not_decode_closes_with_1002_or_1009() {
        for text in [
            "not json",
            "[]",
            "{}",
            r#"{"type":"nope"}"#,
            r#"{"type":"hello"}"#,
            r#"{"type":"hello","heartbeatSecs":0}"#,
            r#"{"type":"hello","heartbeatSecs":20,"x":1}"#,
            r#"{"type":"jobCommand"}"#,
        ] {
            let error = ServerMessage::decode(text).unwrap_err();
            assert_eq!(error.close_code(), CLOSE_CODE_PROTOCOL, "{text}");
        }
        for text in [
            r#"{"type":"hello","heartbeatSecs":20}"#,
            r#"{"type":"commandAck"}"#,
        ] {
            let error = AgentMessage::decode(text).unwrap_err();
            assert_eq!(error.close_code(), CLOSE_CODE_PROTOCOL, "{text}");
        }
        let long = format!(
            r#"{{"type":"commandAck","commandId":"{}"}}"#,
            "x".repeat(MAX_CONTROL_MESSAGE_LEN)
        );
        let error = AgentMessage::decode(&long).unwrap_err();
        assert_eq!(error.close_code(), CLOSE_CODE_TOO_BIG);
    }
}
