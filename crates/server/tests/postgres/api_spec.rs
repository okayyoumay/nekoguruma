//! `api/openapi.yaml` and `api/asyncapi.yaml` against the implementation (design 12, M3 scope:
//! the files are kept in sync with it).
//!
//! The rule, also stated in `api/README.md`: an operation or message the build does not
//! implement carries `x-ngr-status: planned`. Every unmarked one must exist in the code, and
//! everything the code serves or defines must be in the file unmarked. The depth is the route's
//! method and path, and a control message's `type` name with the name and requiredness of each
//! of its properties; value ranges and nested contents are not compared.
//!
//! The code's side is derived, not listed by hand: the served routes are what `router` mounts
//! (`server::router_with_routes`), the message properties are what shared-proto's types
//! serialise to (once with every optional property set, once with none). Every mismatch of a run
//! is reported together.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use server::{AppState, Hasher, Hub, InstructionKey, OperatorSessions, Sessions};
use shared_proto::{
    AgentEventKind, AgentEventMessage, AgentMessage, Checkpoint, HmiRequestBody,
    InstructionRejectReason, JobId, OpenJob, ServerMessage,
};
use tower::ServiceExt;

const PLANNED: &str = "x-ngr-status";

fn yaml(name: &str) -> Value {
    let path = format!("{}/../../api/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_norway::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Whether the node carries the planned mark; any other value of the extension is a mistake.
fn planned(node: &Value, what: &str, errors: &mut Vec<String>) -> bool {
    match node.get(PLANNED) {
        None => false,
        Some(Value::String(s)) if s == "planned" => true,
        Some(other) => {
            errors.push(format!(
                "{what}: {PLANNED} is {other}, the only value is planned"
            ));
            false
        }
    }
}

fn state() -> AppState {
    // The pool never connects: nothing here reaches a handler that queries.
    let pool = sqlx::PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap();
    AppState {
        port: 8080,
        pool,
        instruction_key: Arc::new(InstructionKey::from_signer(Arc::new(
            shared_crypto::SigningKey::generate().unwrap(),
        ))),
        sessions: Sessions::new(),
        hub: Hub::new(),
        operator_sessions: OperatorSessions::new(),
        hasher: Hasher::new(),
    }
}

fn report(errors: Vec<String>) {
    assert!(
        errors.is_empty(),
        "{} mismatch(es) between the API description and the code:\n  - {}",
        errors.len(),
        errors.join("\n  - ")
    );
}

// ------------------------------------------------------------------------------- routes

/// The server's path prefix and the `servers` base of the description: a path in the file is
/// the served path without the prefix, and every `servers` URL must end in it.
const PREFIX: &str = "/api";

#[tokio::test]
async fn the_served_routes_and_openapi_agree() {
    let (_, served) = server::router_with_routes(state());
    let mut errors = Vec::new();

    let spec = yaml("openapi.yaml");
    for s in spec["servers"].as_array().into_iter().flatten() {
        let url = s["url"].as_str().unwrap_or_default();
        if !url.ends_with(PREFIX) {
            errors.push(format!("servers url {url} does not end in {PREFIX}"));
        }
    }

    // The control channel is an upgrade of a GET; asyncapi.yaml describes it, not openapi.yaml.
    let asyncapi = yaml("asyncapi.yaml");
    let channel_addresses: BTreeSet<String> = asyncapi["channels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, c)| c["address"].as_str().map(str::to_owned))
        .collect();

    let mut implemented: BTreeSet<(String, String)> = BTreeSet::new();
    for (method, path) in &served {
        if !path.starts_with(PREFIX) {
            errors.push(format!("served {method} {path} is outside {PREFIX}"));
        }
        if !implemented.insert((method.to_string(), (*path).to_owned())) {
            errors.push(format!("{method} {path} is mounted twice"));
        }
    }

    // Spec side: (METHOD, served path) -> planned.
    let mut described: BTreeMap<(String, String), bool> = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        let path = path.as_str();
        for (method, op) in item.as_object().into_iter().flatten() {
            let method = method.as_str();
            if ![
                "get", "put", "post", "delete", "patch", "head", "options", "trace",
            ]
            .contains(&method)
            {
                continue;
            }
            let upper = method.to_ascii_uppercase();
            let is_planned = planned(op, &format!("{upper} {path}"), &mut errors);
            described.insert((upper, format!("{PREFIX}{path}")), is_planned);
        }
    }

    for (method, path) in &implemented {
        let in_channel = method == "GET" && channel_addresses.contains(path);
        match described.get(&(method.clone(), path.clone())) {
            Some(false) => {}
            Some(true) => errors.push(format!(
                "{method} {path} is served but its operation is marked planned"
            )),
            None if in_channel => {}
            None => errors.push(format!("{method} {path} is served but not in openapi.yaml")),
        }
    }
    for ((method, path), is_planned) in &described {
        if !is_planned && !implemented.contains(&(method.clone(), path.clone())) {
            errors.push(format!(
                "{method} {path} is in openapi.yaml unmarked but not served (mark it {PLANNED}: planned)"
            ));
        }
    }
    report(errors);
}

/// The list holds what the router answers: each route's path exists and takes its method. The
/// route table without the guard answers a method no route takes (DELETE) with 405 and `Allow`,
/// and an unknown path with 404; the control route takes every method and answers for itself.
#[tokio::test]
async fn the_recorded_routes_are_the_routers() {
    let (routes, served) = server::routes(state());
    let mut errors = Vec::new();
    for (method, path) in &served {
        let response = routes
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(*path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if response.status() == StatusCode::NOT_FOUND {
            errors.push(format!(
                "{method} {path} is recorded but the router has no such path"
            ));
            continue;
        }
        if *path == server::control::PATH {
            continue;
        }
        let allow = response
            .headers()
            .get(header::ALLOW)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if response.status() != StatusCode::METHOD_NOT_ALLOWED
            || !allow.split(',').any(|m| m.trim() == method.as_str())
        {
            errors.push(format!(
                "{method} {path}: DELETE got {} with Allow {allow:?}",
                response.status()
            ));
        }
    }
    report(errors);
}

/// Every route goes through `Mounts::mount`, which records it; a bare `.route(` would be served
/// and missing from the list, so the one in `mount` is the only one allowed.
#[test]
fn every_route_is_mounted_through_the_recorder() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs");
    let source = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        source.matches(".route(").count(),
        1,
        "a route is mounted with a bare `.route(` in src/lib.rs: use `Mounts::mount`"
    );
}

// ------------------------------------------------------------------------------ messages

/// One message as the code defines it: its name, the names of its properties and which of them
/// are required (the ones serialised with no optional property set).
#[derive(Debug, PartialEq, Eq)]
struct Shape {
    all: BTreeSet<String>,
    required: BTreeSet<String>,
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn shape(minimal: &impl serde::Serialize, full: &impl serde::Serialize) -> Shape {
    Shape {
        all: keys(&serde_json::to_value(full).unwrap()),
        required: keys(&serde_json::to_value(minimal).unwrap()),
    }
}

fn job() -> JobId {
    JobId("j-1".to_owned())
}

/// A name of a message for the error text and the key of the comparison: the `type`, and for
/// `agentEvent` the `kind` after a slash.
type Name = String;

/// Every message of shared-proto, with a minimal and a full value of each. The matches in
/// `covered_*` have no wildcard arm, so a variant added to a message enum stops compiling here
/// until it has a sample below.
fn code_messages() -> Vec<(Name, Shape)> {
    covered_server(&ServerMessage::Heartbeat);
    covered_agent(&AgentMessage::Heartbeat);
    covered_kind(&AgentEventKind::Progress {
        job_id: job(),
        progress_permille: 0,
        phase: String::new(),
    });

    let event = |seq, at: Option<&str>, event: AgentEventKind| AgentEventMessage {
        seq,
        occurred_at_agent: at.map(str::to_owned),
        event,
    };
    let mut out = vec![
        (
            "hello",
            shape(
                &ServerMessage::Hello { heartbeat_secs: 20 },
                &ServerMessage::Hello { heartbeat_secs: 20 },
            ),
        ),
        (
            "jobCommand",
            shape(
                &ServerMessage::JobCommand {
                    command_id: "c".into(),
                    signed_job: None,
                },
                &ServerMessage::JobCommand {
                    command_id: "c".into(),
                    signed_job: Some("AA==".into()),
                },
            ),
        ),
        (
            "cancelCommand",
            shape(
                &ServerMessage::CancelCommand { job_id: job() },
                &ServerMessage::CancelCommand { job_id: job() },
            ),
        ),
        (
            "hmiResponse",
            shape(
                &ServerMessage::HmiResponse {
                    job_id: job(),
                    request_id: "r".into(),
                    values: None,
                },
                &ServerMessage::HmiResponse {
                    job_id: job(),
                    request_id: "r".into(),
                    values: Some(Default::default()),
                },
            ),
        ),
        (
            "seedKeyResponse",
            shape(
                &ServerMessage::SeedKeyResponse {
                    job_id: job(),
                    request_id: "r".into(),
                    key: "AA==".into(),
                },
                &ServerMessage::SeedKeyResponse {
                    job_id: job(),
                    request_id: "r".into(),
                    key: "AA==".into(),
                },
            ),
        ),
        (
            "eventAck",
            shape(
                &ServerMessage::EventAck { seq: 1 },
                &ServerMessage::EventAck { seq: 1 },
            ),
        ),
        (
            "heartbeat",
            shape(&ServerMessage::Heartbeat, &ServerMessage::Heartbeat),
        ),
        (
            "commandAck",
            shape(
                &AgentMessage::CommandAck {
                    command_id: "c".into(),
                },
                &AgentMessage::CommandAck {
                    command_id: "c".into(),
                },
            ),
        ),
        (
            "commandRejected",
            shape(
                &AgentMessage::CommandRejected {
                    command_id: "c".into(),
                    reason: InstructionRejectReason::Malformed,
                },
                &AgentMessage::CommandRejected {
                    command_id: "c".into(),
                    reason: InstructionRejectReason::Malformed,
                },
            ),
        ),
        (
            "heartbeat",
            shape(&AgentMessage::Heartbeat, &AgentMessage::Heartbeat),
        ),
        (
            "reconcile",
            shape(
                &AgentMessage::Reconcile { open_jobs: vec![] },
                &AgentMessage::Reconcile {
                    open_jobs: vec![OpenJob {
                        job_id: job(),
                        state: "running".into(),
                        checkpoint: Some(Checkpoint {
                            section: Some(1),
                            vin: None,
                            artifact_digest: None,
                        }),
                    }],
                },
            ),
        ),
    ]
    .into_iter()
    .map(|(n, s)| (n.to_owned(), s))
    .collect::<Vec<_>>();

    let kinds = [
        (
            "jobState",
            AgentEventKind::JobState {
                job_id: job(),
                state: "running".into(),
                failure_code: None,
                procedure_code: None,
                detail: None,
            },
            AgentEventKind::JobState {
                job_id: job(),
                state: "failed".into(),
                failure_code: Some("procedure".into()),
                procedure_code: Some("p".into()),
                detail: Some("d".into()),
            },
        ),
        (
            "progress",
            AgentEventKind::Progress {
                job_id: job(),
                progress_permille: 1,
                phase: "p".into(),
            },
            AgentEventKind::Progress {
                job_id: job(),
                progress_permille: 1,
                phase: "p".into(),
            },
        ),
        (
            "hmiRequest",
            AgentEventKind::HmiRequest {
                job_id: job(),
                hmi_request: HmiRequestBody {
                    request_id: "r".into(),
                    template_id: "t".into(),
                },
            },
            AgentEventKind::HmiRequest {
                job_id: job(),
                hmi_request: HmiRequestBody {
                    request_id: "r".into(),
                    template_id: "t".into(),
                },
            },
        ),
    ];
    for (kind, min, full) in kinds {
        // The message is the event wrapped as the agent sends it (`type` is the enum's tag).
        let min = AgentMessage::AgentEvent(event(None, None, min));
        let full = AgentMessage::AgentEvent(event(Some(1), Some("t"), full));
        out.push((format!("agentEvent/{kind}"), shape(&min, &full)));
    }
    out
}

fn covered_server(m: &ServerMessage) {
    match m {
        ServerMessage::Hello { .. }
        | ServerMessage::JobCommand { .. }
        | ServerMessage::CancelCommand { .. }
        | ServerMessage::HmiResponse { .. }
        | ServerMessage::SeedKeyResponse { .. }
        | ServerMessage::EventAck { .. }
        | ServerMessage::Heartbeat => {}
    }
}
fn covered_agent(m: &AgentMessage) {
    match m {
        AgentMessage::CommandAck { .. }
        | AgentMessage::CommandRejected { .. }
        | AgentMessage::AgentEvent(_)
        | AgentMessage::Heartbeat
        | AgentMessage::Reconcile { .. } => {}
    }
}
fn covered_kind(k: &AgentEventKind) {
    match k {
        AgentEventKind::JobState { .. }
        | AgentEventKind::Progress { .. }
        | AgentEventKind::HmiRequest { .. } => {}
    }
}

/// The kinds of `agentEvent` whose `seq` is numbered: the type makes `seq` optional (a progress
/// event has none), the description requires it for these two (5.3, ADR-478).
const NUMBERED_KINDS: [&str; 2] = ["jobState", "hmiRequest"];

fn string_set(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

fn property_names(v: &Value) -> BTreeSet<String> {
    v["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k.clone())
        .collect()
}

/// The messages of asyncapi.yaml as `(name, shape, planned)`; `agentEvent` gives one per kind
/// from the `oneOf` branches, each with the top-level properties added.
fn spec_messages(errors: &mut Vec<String>) -> Vec<(Name, Option<Shape>, bool)> {
    let spec = yaml("asyncapi.yaml");
    let mut out = Vec::new();
    for (key, message) in spec["components"]["messages"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let key = key.as_str();
        let is_planned = planned(message, &format!("message {key}"), errors);
        let payload = &message["payload"];
        let Some(ty) = payload["properties"]["type"]["const"].as_str() else {
            // A browser-stream message: no `type`, so no control message to compare with.
            out.push((key.to_owned(), None, is_planned));
            continue;
        };
        if let Some(branches) = payload["oneOf"].as_array() {
            let top_all = property_names(payload);
            let top_required = string_set(&payload["required"]);
            for branch in branches {
                let Some(kind) = branch["properties"]["kind"]["const"].as_str() else {
                    errors.push(format!("message {key}: a oneOf branch has no kind const"));
                    continue;
                };
                let mut all = top_all.clone();
                all.extend(property_names(branch));
                let mut required = top_required.clone();
                required.extend(string_set(&branch["required"]));
                out.push((
                    format!("{ty}/{kind}"),
                    Some(Shape { all, required }),
                    is_planned,
                ));
            }
        } else {
            out.push((
                ty.to_owned(),
                Some(Shape {
                    all: property_names(payload),
                    required: string_set(&payload["required"]),
                }),
                is_planned,
            ));
        }
    }
    out
}

#[test]
fn the_control_messages_and_asyncapi_agree() {
    let mut errors = Vec::new();
    let spec = spec_messages(&mut errors);

    // A name may exist in both directions (heartbeat); the definitions must be the same.
    let mut code: BTreeMap<Name, Shape> = BTreeMap::new();
    for (name, shape) in code_messages() {
        match code.get(&name) {
            Some(first) if *first != shape => errors.push(format!(
                "{name}: the two directions define different properties"
            )),
            Some(_) => {}
            None => {
                code.insert(name, shape);
            }
        }
    }

    let mut described: BTreeSet<&str> = BTreeSet::new();
    for (name, shape, is_planned) in &spec {
        let Some(shape) = shape else {
            if !is_planned {
                errors.push(format!(
                    "message {name} has no `type` const, so no control message can match it: mark it {PLANNED}: planned"
                ));
            }
            continue;
        };
        match (code.get(name), is_planned) {
            (None, true) => {}
            (None, false) => errors.push(format!(
                "{name} is in asyncapi.yaml unmarked but shared-proto does not define it (mark it {PLANNED}: planned)"
            )),
            (Some(_), true) => errors.push(format!(
                "{name} is defined by shared-proto but its message is marked planned"
            )),
            (Some(code_shape), false) => {
                described.insert(name);
                let mut required = code_shape.required.clone();
                if let Some(kind) = name.strip_prefix("agentEvent/")
                    && NUMBERED_KINDS.contains(&kind)
                {
                    required.insert("seq".to_owned());
                }
                for p in code_shape.all.difference(&shape.all) {
                    errors.push(format!("{name}: property {p} is in the code, not in the file"));
                }
                for p in shape.all.difference(&code_shape.all) {
                    errors.push(format!("{name}: property {p} is in the file, not in the code"));
                }
                for p in required.symmetric_difference(&shape.required) {
                    if code_shape.all.contains(p) && shape.all.contains(p) {
                        let side = if required.contains(p) { "the code" } else { "the file" };
                        errors.push(format!("{name}: property {p} is required by {side} only"));
                    }
                }
            }
        }
    }
    for name in code.keys() {
        if !described.contains(name.as_str()) {
            errors.push(format!(
                "{name} is defined by shared-proto but not in asyncapi.yaml unmarked"
            ));
        }
    }
    report(errors);
}

// Keeps `json!` used for a fixture of the self-test below.
#[test]
fn the_planned_mark_accepts_only_its_one_value() {
    let mut errors = Vec::new();
    assert!(planned(&json!({PLANNED: "planned"}), "x", &mut errors));
    assert!(!planned(&json!({}), "x", &mut errors));
    assert!(errors.is_empty());
    assert!(!planned(&json!({PLANNED: "later"}), "x", &mut errors));
    assert_eq!(errors.len(), 1);
}
