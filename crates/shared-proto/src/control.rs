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

use crate::{Capabilities, InstructionRejectReason, JobId};

/// The WebSocket subprotocol of this version of the channel. An upgrade that does not offer it
/// is refused (ADR-466).
pub const CONTROL_SUBPROTOCOL: &str = "ngr-control.v1";

/// The longest message, in bytes of its JSON text, in either direction (ADR-466). A full job
/// command (a 64 KiB envelope in base64) is about 87.6 KiB.
pub const MAX_CONTROL_MESSAGE_LEN: usize = 128 * 1024;

/// The heartbeat interval the server names in `hello` unless configured otherwise (14.1).
pub const DEFAULT_HEARTBEAT_SECS: u32 = 20;

/// How many intervals without any frame a side waits before it closes the connection (14.1).
pub const HEARTBEAT_MISSED_LIMIT: u32 = 3;

/// The application close code with which the server ends a connection that a newer connection
/// of the same agent replaced (ADR-466).
pub const CLOSE_CODE_REPLACED: u16 = 4001;

/// A message could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The text is longer than [`MAX_CONTROL_MESSAGE_LEN`].
    #[error("a control message is at most {MAX_CONTROL_MESSAGE_LEN} bytes")]
    TooLarge,
    /// The text is not a message of this version: not JSON, no known `type`, or a property of
    /// the wrong form. The connection is closed.
    #[error("not a control message of this version")]
    Malformed(#[source] serde_json::Error),
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
        signed_job: String,
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
    /// Every event up to and including `seq` is stored. Reserved for the outbox work.
    EventAck {
        seq: u64,
    },
    /// The echo of the agent's heartbeat.
    Heartbeat,
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

/// An event with its place in the agent's outbox (5.3). The properties of the [`AgentEventKind`]
/// sit beside `seq` in the same object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEventMessage {
    /// Increases monotonically per agent.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// Device time, for reference only (16.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_agent: Option<String>,
    #[serde(flatten)]
    pub event: AgentEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    Guaranteed,
    Lossy,
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
    /// Keeps the field names of [`Capabilities`] as they are.
    Capabilities { capabilities: Box<Capabilities> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HmiRequestBody {
    pub request_id: String,
    /// Record template or HMI form.
    pub template_id: String,
}

/// One open job in a reconciliation (5.3); only open jobs are sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenJob {
    pub job_id: JobId,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
}

/// Summary for handover to another device (8.2.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
}

fn decode<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, ControlError> {
    if text.len() > MAX_CONTROL_MESSAGE_LEN {
        return Err(ControlError::TooLarge);
    }
    serde_json::from_str(text).map_err(ControlError::Malformed)
}

fn encode<T: Serialize>(message: &T) -> Result<String, ControlError> {
    let text = serde_json::to_string(message).map_err(ControlError::Malformed)?;
    if text.len() > MAX_CONTROL_MESSAGE_LEN {
        return Err(ControlError::TooLarge);
    }
    Ok(text)
}

impl ServerMessage {
    /// Reads one text frame. Any error closes the connection.
    pub fn decode(text: &str) -> Result<Self, ControlError> {
        decode(text)
    }

    /// Writes one text frame, refusing one over the limit.
    pub fn encode(&self) -> Result<String, ControlError> {
        encode(self)
    }
}

impl AgentMessage {
    /// Reads one text frame. Any error closes the connection.
    pub fn decode(text: &str) -> Result<Self, ControlError> {
        decode(text)
    }

    /// Writes one text frame, refusing one over the limit.
    pub fn encode(&self) -> Result<String, ControlError> {
        encode(self)
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
        text.lines()
            .filter_map(|line| line.trim_start().strip_prefix("- payload: "))
            .map(|json| serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}")))
            .collect()
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
        assert!(text.contains(&CLOSE_CODE_REPLACED.to_string()));
        assert!(text.contains(&format!("{DEFAULT_HEARTBEAT_SECS} by default")));
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
    fn an_unknown_type_or_shape_is_malformed() {
        for text in [
            r#"{"type":"nope"}"#,
            r#"{"heartbeatSecs":20}"#,
            r#"{"type":"hello"}"#,
            r#"{"type":"hello","heartbeatSecs":"20"}"#,
            r#"["hello"]"#,
            "not json",
            "",
        ] {
            assert!(
                matches!(ServerMessage::decode(text), Err(ControlError::Malformed(_))),
                "{text}"
            );
        }
        // A server message is not an agent message, and the reverse.
        assert!(AgentMessage::decode(r#"{"type":"hello","heartbeatSecs":20}"#).is_err());
        assert!(ServerMessage::decode(r#"{"type":"commandAck","commandId":"c"}"#).is_err());
        // An agent event of an unknown kind.
        assert!(AgentMessage::decode(r#"{"type":"agentEvent","seq":1,"kind":"capture"}"#).is_err());
    }

    #[test]
    fn a_payload_carries_no_agent_id() {
        // The identity is the session's: a stray agentId is ignored, never read.
        let m = AgentMessage::decode(r#"{"type":"heartbeat","agentId":"other"}"#).unwrap();
        assert_eq!(m.encode().unwrap(), r#"{"type":"heartbeat"}"#);
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
            signed_job: big,
        };
        assert!(matches!(m.encode(), Err(ControlError::TooLarge)));
        // A full envelope in base64 fits: 65,633 bytes are 87,512 characters.
        let full = ServerMessage::JobCommand {
            command_id: "c".into(),
            signed_job: "A".repeat(87_512),
        };
        assert!(full.encode().is_ok());
    }

    #[test]
    fn capabilities_events_round_trip() {
        let caps = Capabilities {
            agent_version: "0.1.0".into(),
            protocol_version: crate::PROTOCOL_VERSION,
            platform: crate::Platform {
                os: "linux".into(),
                native_arch: "x86_64".into(),
                emulated: false,
                mode: crate::AgentMode::Device,
            },
            workers: vec![],
            vcis: vec![],
            ir_schema_versions: vec![1],
            extension_schema_versions: vec![],
            monitor_direct: false,
            offline_capable: true,
        };
        let m = AgentMessage::AgentEvent(AgentEventMessage {
            seq: 1,
            delivery: Some(Delivery::Guaranteed),
            occurred_at_agent: None,
            event: AgentEventKind::Capabilities {
                capabilities: Box::new(caps),
            },
        });
        let text = m.encode().unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["type"], "agentEvent");
        assert_eq!(v["kind"], "capabilities");
        assert_eq!(v["capabilities"]["agent_version"], "0.1.0");
        assert_eq!(
            serde_json::to_string(&AgentMessage::decode(&text).unwrap()).unwrap(),
            text
        );
    }
}
