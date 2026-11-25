//! `api/openapi.yaml` and `api/asyncapi.yaml` against the implementation (design 12, M3 scope:
//! the files are kept in sync with it).
//!
//! The rule, also stated in `api/README.md`: an operation or message the build does not
//! implement carries `x-ngr-status: planned`. Every unmarked one must exist in the code, and
//! everything the code serves or defines must be in the file unmarked. The depth is the route's
//! method and path, and a control message's `type` name (and `kind` for `agentEvent`) with the
//! name and requiredness of each of its properties; value ranges and nested contents are not
//! compared.
//!
//! The code's side is derived, not listed by hand: the served routes are what `router` mounts
//! (`server::router_with_routes`), and a message's properties are the keys of a full sample that
//! went through shared-proto's own `encode`; a property is required when the real `decode`
//! refuses the sample without it. The comparisons are functions of the two sides, with
//! self-tests on small fixtures, and every mismatch of a run is reported together.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use server::{AppState, Hasher, Hub, InstructionKey, OperatorSessions, Sessions};
use shared_proto::{
    AgentEventKind, AgentEventMessage, AgentMessage, Checkpoint, HmiRequestBody,
    InstructionRejectReason, JobId, LogUploadOutcome, OpenJob, ServerMessage,
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
        uploads: server::uploads::Backend::default(),
        completions: server::uploads::Completions::default(),
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

type Route = (String, String);

/// The routes of `openapi.yaml` as `(METHOD, served path) -> planned`, with the problems found
/// reading it.
fn spec_routes(spec: &Value, errors: &mut Vec<String>) -> BTreeMap<Route, bool> {
    for s in spec["servers"].as_array().into_iter().flatten() {
        let url = s["url"].as_str().unwrap_or_default();
        if !url.ends_with(PREFIX) {
            errors.push(format!("servers url {url} does not end in {PREFIX}"));
        }
    }
    let mut described = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for (method, op) in item.as_object().into_iter().flatten() {
            if ![
                "get", "put", "post", "delete", "patch", "head", "options", "trace",
            ]
            .contains(&method.as_str())
            {
                continue;
            }
            let upper = method.to_ascii_uppercase();
            let is_planned = planned(op, &format!("{upper} {path}"), errors);
            described.insert((upper, format!("{PREFIX}{path}")), is_planned);
        }
    }
    described
}

/// Compares the served routes with the description. `channel_addresses` are the addresses of
/// `asyncapi.yaml`'s channels: the control channel's upgrade is a GET described there.
fn compare_routes(
    served: &[Route],
    spec: &BTreeMap<Route, bool>,
    channel_addresses: &BTreeSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    let mut implemented: BTreeSet<&Route> = BTreeSet::new();
    for route @ (method, path) in served {
        if !path.starts_with(PREFIX) {
            errors.push(format!("served {method} {path} is outside {PREFIX}"));
        }
        if !implemented.insert(route) {
            errors.push(format!("{method} {path} is mounted twice"));
        }
    }
    for route @ (method, path) in &implemented {
        let in_channel = method == "GET" && channel_addresses.contains(path);
        match spec.get(*route) {
            Some(false) => {}
            Some(true) => errors.push(format!(
                "{method} {path} is served but its operation is marked planned"
            )),
            None if in_channel => {}
            None => errors.push(format!("{method} {path} is served but not in openapi.yaml")),
        }
    }
    for (route @ (method, path), is_planned) in spec {
        if !is_planned && !implemented.contains(route) {
            errors.push(format!(
                "{method} {path} is in openapi.yaml unmarked but not served (mark it {PLANNED}: planned)"
            ));
        }
    }
    errors
}

fn served_routes(served: &[(Method, &'static str)]) -> Vec<Route> {
    served
        .iter()
        .map(|(m, p)| (m.to_string(), (*p).to_owned()))
        .collect()
}

#[tokio::test]
async fn the_served_routes_and_openapi_agree() {
    let (_, served) = server::router_with_routes(state());
    let mut errors = Vec::new();
    let spec = spec_routes(&yaml("openapi.yaml"), &mut errors);
    // The control channel is an upgrade of a GET; asyncapi.yaml describes it, not openapi.yaml.
    let asyncapi = yaml("asyncapi.yaml");
    let channel_addresses: BTreeSet<String> = asyncapi["channels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, c)| c["address"].as_str().map(str::to_owned))
        .collect();
    errors.extend(compare_routes(
        &served_routes(&served),
        &spec,
        &channel_addresses,
    ));
    report(errors);
}

async fn status_and_allow(
    routes: &axum::Router,
    method: &Method,
    path: &str,
) -> (StatusCode, BTreeSet<String>) {
    let response = routes
        .clone()
        .oneshot(
            Request::builder()
                .method(method.clone())
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let allow = response
        .headers()
        .get(header::ALLOW)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect();
    (response.status(), allow)
}

/// The recorded list is what the router answers: per path, a method none of its recorded
/// routes takes is answered 405 with an `Allow` that is exactly the recorded methods (and HEAD
/// where a GET is recorded, as axum adds it; the control route refuses HEAD itself and lists
/// GET only). A planned path, with a dummy for each parameter, is answered by the table's 404.
#[tokio::test]
async fn the_recorded_routes_are_the_routers() {
    let (routes, served) = server::routes_unguarded(state());
    let mut errors = Vec::new();
    let mut by_path: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (method, path) in &served {
        by_path.entry(path).or_default().insert(method.to_string());
    }
    for (path, methods) in &by_path {
        let probe = [
            Method::DELETE,
            Method::PATCH,
            Method::PUT,
            Method::POST,
            Method::GET,
        ]
        .into_iter()
        .find(|m| !methods.contains(m.as_str()))
        .unwrap();
        let mut expected = methods.clone();
        if methods.contains("GET") && *path != server::control::PATH {
            expected.insert("HEAD".to_owned());
        }
        let (status, allow) = status_and_allow(&routes, &probe, path).await;
        if status != StatusCode::METHOD_NOT_ALLOWED || allow != expected {
            errors.push(format!(
                "{path}: {probe} got {status} with Allow {allow:?}, expected 405 with {expected:?}"
            ));
        }
    }

    let spec = spec_routes(&yaml("openapi.yaml"), &mut errors);
    for ((method, path), is_planned) in &spec {
        if !is_planned {
            continue;
        }
        let mut dummy = String::new();
        let mut in_param = false;
        for c in path.chars() {
            match c {
                '{' => {
                    in_param = true;
                    dummy.push_str("dummy");
                }
                '}' => in_param = false,
                c if !in_param => dummy.push(c),
                _ => {}
            }
        }
        let method: Method = method.parse().unwrap();
        let (status, _) = status_and_allow(&routes, &method, &dummy).await;
        if status != StatusCode::NOT_FOUND {
            errors.push(format!(
                "{method} {path} is marked planned but {dummy} is answered {status}, not 404"
            ));
        }
    }
    report(errors);
}

/// Where the router may be built: every route goes through `Mounts::mount`, which records it, so
/// a route added any other way would be served and missing from the list. Each file under
/// `src` may use the calls that add routes or routers only as often as listed (the test module
/// of `operator_session.rs` builds a router of its own); a new use fails until it is reviewed
/// and listed here.
#[test]
fn routes_are_mounted_only_where_they_are_recorded() {
    const CALLS: [&str; 6] = [
        ".route(",
        ".route_service(",
        ".nest(",
        ".nest_service(",
        ".fallback_service(",
        ".merge(",
    ];
    // (file under src, call, count)
    const ALLOWED: [(&str, &str, usize); 4] = [
        ("lib.rs", ".route(", 1),
        ("lib.rs", ".fallback_service(", 1),
        ("lib.rs", ".merge(", 8),
        ("operator_session.rs", ".route(", 1),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.clone()];
    let mut found: BTreeMap<(String, &str), usize> = BTreeMap::new();
    while let Some(entry) = files.pop() {
        if entry.is_dir() {
            files.extend(
                std::fs::read_dir(&entry)
                    .unwrap()
                    .map(|e| e.unwrap().path()),
            );
        } else if entry.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&entry).unwrap();
            let name = entry
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for call in CALLS {
                let n = text.matches(call).count();
                if n > 0 {
                    found.insert((name.clone(), call), n);
                }
            }
        }
    }
    let mut errors = Vec::new();
    for ((file, call), n) in &found {
        let allowed = ALLOWED
            .iter()
            .find(|(f, c, _)| f == file && c == call)
            .map_or(0, |(_, _, n)| *n);
        if *n != allowed {
            errors.push(format!(
                "src/{file} has {n} use(s) of `{call}`, the list allows {allowed}: mount routes with `Mounts::mount` in lib.rs"
            ));
        }
    }
    for (file, call, n) in ALLOWED {
        if n > 0 && !found.contains_key(&(file.to_owned(), call)) {
            errors.push(format!(
                "the list allows `{call}` in src/{file}, which has none"
            ));
        }
    }
    report(errors);
}

// ------------------------------------------------------------------------------ messages

/// One message as the code defines it: the names of its properties (besides `type`, which is the
/// message's name) and which of them are required.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Shape {
    all: BTreeSet<String>,
    required: BTreeSet<String>,
}

/// A message's name: its `type`, and for `agentEvent` its `kind` after a slash.
type Name = String;

#[derive(Clone)]
enum Msg {
    Server(ServerMessage),
    Agent(AgentMessage),
}

impl Msg {
    fn encode(&self) -> String {
        match self {
            Msg::Server(m) => m.encode(),
            Msg::Agent(m) => m.encode(),
        }
        .unwrap_or_else(|e| panic!("a sample that does not encode: {e}"))
    }

    fn decodes(&self, text: &str) -> bool {
        match self {
            Msg::Server(_) => ServerMessage::decode(text).is_ok(),
            Msg::Agent(_) => AgentMessage::decode(text).is_ok(),
        }
    }
}

/// A message with every optional property set, and one with none.
struct Sample {
    min: Msg,
    full: Msg,
}

fn job() -> JobId {
    JobId("j-1".to_owned())
}

fn server(min: ServerMessage, full: ServerMessage) -> Sample {
    Sample {
        min: Msg::Server(min),
        full: Msg::Server(full),
    }
}

fn agent(min: AgentMessage, full: AgentMessage) -> Sample {
    Sample {
        min: Msg::Agent(min),
        full: Msg::Agent(full),
    }
}

/// Defines `$name`, which returns the samples of every entry. Each entry's pattern also goes
/// into an exhaustive `match` with no wildcard arm, so a variant added to the enum stops
/// compiling here until an entry gives it samples: a variant cannot be left out.
macro_rules! samples {
    ($name:ident, $ty:ty, $out:ty; $($pat:pat => $samples:expr;)*) => {
        fn $name() -> Vec<$out> {
            #[expect(dead_code, reason = "it exists to be compiled: the match must be exhaustive")]
            fn exhaustive(m: &$ty) {
                match m {
                    $($pat => {})*
                }
            }
            let mut out = Vec::new();
            $(out.extend($samples);)*
            out
        }
    };
}

samples! { server_samples, ServerMessage, Sample;
    ServerMessage::Hello { .. } => {
        let hello = ServerMessage::Hello { heartbeat_secs: 20 };
        vec![server(hello.clone(), hello)]
    };
    ServerMessage::JobCommand { .. } => vec![server(
        ServerMessage::JobCommand { command_id: "c".into(), signed_job: None },
        ServerMessage::JobCommand { command_id: "c".into(), signed_job: Some("AA==".into()) },
    )];
    ServerMessage::CancelCommand { .. } => {
        let m = ServerMessage::CancelCommand { job_id: job() };
        vec![server(m.clone(), m)]
    };
    ServerMessage::HmiResponse { .. } => vec![server(
        ServerMessage::HmiResponse { job_id: job(), request_id: "r".into(), values: None },
        ServerMessage::HmiResponse {
            job_id: job(),
            request_id: "r".into(),
            values: Some(Default::default()),
        },
    )];
    ServerMessage::SeedKeyResponse { .. } => {
        let m = ServerMessage::SeedKeyResponse {
            job_id: job(),
            request_id: "r".into(),
            key: "AA==".into(),
        };
        vec![server(m.clone(), m)]
    };
    ServerMessage::EventAck { .. } => {
        let m = ServerMessage::EventAck { seq: 1 };
        vec![server(m.clone(), m)]
    };
    ServerMessage::LogUploadCommand { .. } => {
        let m = ServerMessage::LogUploadCommand { job_id: job(), generation: 1 };
        vec![server(m.clone(), m)]
    };
    ServerMessage::Heartbeat => vec![server(ServerMessage::Heartbeat, ServerMessage::Heartbeat)];
}

// The minimal and the full value of each kind of event.
samples! { kind_samples, AgentEventKind, (AgentEventKind, AgentEventKind);
    AgentEventKind::JobState { .. } => vec![(
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
    )];
    AgentEventKind::Progress { .. } => {
        let k = AgentEventKind::Progress {
            job_id: job(),
            progress_permille: 1,
            phase: "p".into(),
        };
        vec![(k.clone(), k)]
    };
    AgentEventKind::HmiRequest { .. } => {
        let k = AgentEventKind::HmiRequest {
            job_id: job(),
            hmi_request: HmiRequestBody { request_id: "r".into(), template_id: "t".into() },
        };
        vec![(k.clone(), k)]
    };
}

/// The event's samples. The wrapper's `seq` and `occurredAtAgent` are set where the real
/// `encode` allows it: it refuses a progress event with a `seq` and any other without one.
fn event_samples() -> Vec<Sample> {
    kind_samples()
        .into_iter()
        .map(|(min, full)| {
            let numbered = !matches!(min, AgentEventKind::Progress { .. });
            let wrap = |seq, at: Option<&str>, event| {
                AgentMessage::AgentEvent(AgentEventMessage {
                    seq,
                    occurred_at_agent: at.map(str::to_owned),
                    event,
                })
            };
            agent(
                wrap(numbered.then_some(1), None, min),
                wrap(numbered.then_some(1), Some("2026-01-01T00:00:00Z"), full),
            )
        })
        .collect()
}

samples! { agent_samples, AgentMessage, Sample;
    AgentMessage::CommandAck { .. } => {
        let m = AgentMessage::CommandAck { command_id: "c".into() };
        vec![agent(m.clone(), m)]
    };
    AgentMessage::CommandRejected { .. } => {
        let m = AgentMessage::CommandRejected {
            command_id: "c".into(),
            reason: InstructionRejectReason::Malformed,
        };
        vec![agent(m.clone(), m)]
    };
    AgentMessage::AgentEvent(_) => event_samples();
    AgentMessage::Heartbeat => vec![agent(AgentMessage::Heartbeat, AgentMessage::Heartbeat)];
    AgentMessage::LogUploadAnswer { .. } => {
        let m = AgentMessage::LogUploadAnswer {
            job_id: job(),
            generation: 1,
            outcome: LogUploadOutcome::Queued,
        };
        vec![agent(m.clone(), m)]
    };
    AgentMessage::Reconcile { .. } => vec![agent(
        AgentMessage::Reconcile { open_jobs: vec![] },
        AgentMessage::Reconcile {
            open_jobs: vec![OpenJob {
                job_id: job(),
                state: "running".into(),
                checkpoint: Some(Checkpoint { section: Some(1), vin: None, artifact_digest: None }),
            }],
        },
    )];
}

fn keys_of(text: &str) -> BTreeSet<String> {
    serde_json::from_str::<Value>(text)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "type")
        .cloned()
        .collect()
}

/// The name, the shape and the problems of one sample.
fn derive(sample: &Sample, errors: &mut Vec<String>) -> (Name, Shape) {
    let name_of = |text: &str| {
        let v: Value = serde_json::from_str(text).unwrap();
        let ty = v["type"].as_str().unwrap().to_owned();
        match v["kind"].as_str() {
            Some(kind) if ty == "agentEvent" => format!("{ty}/{kind}"),
            _ => ty,
        }
    };
    let full = sample.full.encode();
    let min = sample.min.encode();
    let name = name_of(&full);
    if name_of(&min) != name {
        errors.push(format!("{name}: the minimal sample is {}", name_of(&min)));
    }
    let all = keys_of(&full);
    // A property is required when the real decoder refuses the full message without it.
    let mut required = BTreeSet::new();
    for key in &all {
        let mut value: Value = serde_json::from_str(&full).unwrap();
        value.as_object_mut().unwrap().remove(key);
        if !sample.full.decodes(&value.to_string()) {
            required.insert(key.clone());
        }
    }
    if keys_of(&min) != required {
        errors.push(format!(
            "{name}: the minimal sample has {:?} but the decoder requires {required:?}",
            keys_of(&min)
        ));
    }
    (name, Shape { all, required })
}

fn code_messages(errors: &mut Vec<String>) -> BTreeMap<Name, Shape> {
    let mut samples = server_samples();
    samples.extend(agent_samples());

    // A name may exist in both directions (heartbeat); the definitions must be the same.
    let mut code: BTreeMap<Name, Shape> = BTreeMap::new();
    for sample in &samples {
        let (name, shape) = derive(sample, errors);
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
    code
}

fn string_set(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// A schema object's own properties and required list, `type` left out.
fn shape_of(schema: &Value) -> Shape {
    let without_type = |mut set: BTreeSet<String>| {
        set.remove("type");
        set
    };
    Shape {
        all: without_type(
            schema["properties"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.clone())
                .collect(),
        ),
        required: without_type(string_set(&schema["required"])),
    }
}

struct SpecMessage {
    name: Name,
    /// None for a message with no `type` (the browser stream's).
    shape: Option<Shape>,
    planned: bool,
}

/// The messages of asyncapi.yaml; `agentEvent` gives one per `oneOf` branch, each with the
/// branch's own properties and required list (a branch says which of the top-level ones it has).
fn spec_messages(spec: &Value, errors: &mut Vec<String>) -> Vec<SpecMessage> {
    let mut out = Vec::new();
    for (key, message) in spec["components"]["messages"]
        .as_object()
        .into_iter()
        .flatten()
    {
        let is_planned = planned(message, &format!("message {key}"), errors);
        let payload = &message["payload"];
        let Some(ty) = payload["properties"]["type"]["const"].as_str() else {
            out.push(SpecMessage {
                name: key.clone(),
                shape: None,
                planned: is_planned,
            });
            continue;
        };
        if let Some(branches) = payload["oneOf"].as_array() {
            for branch in branches {
                let Some(kind) = branch["properties"]["kind"]["const"].as_str() else {
                    errors.push(format!("message {key}: a oneOf branch has no kind const"));
                    continue;
                };
                out.push(SpecMessage {
                    name: format!("{ty}/{kind}"),
                    shape: Some(shape_of(branch)),
                    planned: is_planned,
                });
            }
        } else {
            out.push(SpecMessage {
                name: ty.to_owned(),
                shape: Some(shape_of(payload)),
                planned: is_planned,
            });
        }
    }
    out
}

fn compare_messages(code: &BTreeMap<Name, Shape>, spec: &[SpecMessage]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut described: BTreeSet<&str> = BTreeSet::new();
    for SpecMessage {
        name,
        shape,
        planned,
    } in spec
    {
        let Some(shape) = shape else {
            if !planned {
                errors.push(format!(
                    "message {name} has no `type` const, so no control message can match it: mark it {PLANNED}: planned"
                ));
            }
            continue;
        };
        match (code.get(name), planned) {
            (None, true) => {}
            (None, false) => errors.push(format!(
                "{name} is in asyncapi.yaml unmarked but shared-proto does not define it (mark it {PLANNED}: planned)"
            )),
            (Some(_), true) => errors.push(format!(
                "{name} is defined by shared-proto but its message is marked planned"
            )),
            (Some(code_shape), false) => {
                described.insert(name);
                for p in code_shape.all.difference(&shape.all) {
                    errors.push(format!(
                        "{name}: property {p} is in the code, not in the file"
                    ));
                }
                for p in shape.all.difference(&code_shape.all) {
                    errors.push(format!(
                        "{name}: property {p} is in the file, not in the code"
                    ));
                }
                for p in code_shape.required.symmetric_difference(&shape.required) {
                    if code_shape.all.contains(p) && shape.all.contains(p) {
                        let side = if code_shape.required.contains(p) {
                            "the code"
                        } else {
                            "the file"
                        };
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
    errors
}

#[test]
fn the_control_messages_and_asyncapi_agree() {
    let mut errors = Vec::new();
    let code = code_messages(&mut errors);
    let spec = spec_messages(&yaml("asyncapi.yaml"), &mut errors);
    errors.extend(compare_messages(&code, &spec));
    report(errors);
}

// ------------------------------------------------------------------------------ self-tests

fn route(m: &str, p: &str) -> Route {
    (m.to_owned(), p.to_owned())
}

fn spec_of(routes: &[(&str, &str, bool)]) -> BTreeMap<Route, bool> {
    routes
        .iter()
        .map(|(m, p, planned)| (route(m, p), *planned))
        .collect()
}

#[test]
fn a_route_check_names_each_kind_of_drift() {
    let none = BTreeSet::new();
    let spec = spec_of(&[("GET", "/api/v1/a", false), ("POST", "/api/v1/b", true)]);
    assert!(compare_routes(&[route("GET", "/api/v1/a")], &spec, &none).is_empty());

    // Served, not in the file.
    let e = compare_routes(
        &[route("GET", "/api/v1/a"), route("GET", "/api/v1/extra")],
        &spec,
        &none,
    );
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("served but not in openapi.yaml"));
    // In the file unmarked, not served.
    let e = compare_routes(&[], &spec, &none);
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("unmarked but not served"));
    // Served and marked planned; the right path with another method.
    let e = compare_routes(
        &[route("GET", "/api/v1/a"), route("POST", "/api/v1/b")],
        &spec,
        &none,
    );
    assert!(e.iter().any(|e| e.contains("marked planned")), "{e:?}");
    let e = compare_routes(&[route("POST", "/api/v1/a")], &spec, &none);
    assert_eq!(e.len(), 2, "{e:?}");
    // A channel's address stands in for a spec entry.
    let channels: BTreeSet<String> = ["/api/v1/ws".to_owned()].into();
    assert!(
        compare_routes(
            &[route("GET", "/api/v1/a"), route("GET", "/api/v1/ws")],
            &spec,
            &channels
        )
        .is_empty()
    );
}

fn shape(all: &[&str], required: &[&str]) -> Shape {
    Shape {
        all: all.iter().map(|s| (*s).to_owned()).collect(),
        required: required.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn message(name: &str, shape: Shape, planned: bool) -> SpecMessage {
    SpecMessage {
        name: name.to_owned(),
        shape: Some(shape),
        planned,
    }
}

#[test]
fn a_message_check_names_each_kind_of_drift() {
    let code: BTreeMap<Name, Shape> = [("m".to_owned(), shape(&["a", "b"], &["a"]))].into();
    assert!(compare_messages(&code, &[message("m", shape(&["a", "b"], &["a"]), false)]).is_empty());
    let one = |spec: SpecMessage| compare_messages(&code, &[spec]);

    // A property the file lacks, a property the code lacks, a required mismatch each way.
    assert!(one(message("m", shape(&["a"], &["a"]), false))[0].contains("not in the file"));
    assert!(
        one(message("m", shape(&["a", "b", "c"], &["a"]), false))[0].contains("not in the code")
    );
    assert!(one(message("m", shape(&["a", "b"], &["a", "b"]), false))[0].contains("file only"));
    assert!(one(message("m", shape(&["a", "b"], &[]), false))[0].contains("code only"));
    // In the code, not in the file at all; in the file unmarked, not in the code; marked planned
    // while defined; a message with no type left unmarked.
    assert!(compare_messages(&code, &[])[0].contains("not in asyncapi.yaml"));
    assert!(
        one(message("n", shape(&[], &[]), false))
            .iter()
            .any(|e| e.contains("does not define"))
    );
    assert!(one(message("m", shape(&["a", "b"], &["a"]), true))[0].contains("marked planned"));
    let untyped = SpecMessage {
        name: "x".to_owned(),
        shape: None,
        planned: false,
    };
    assert!(one(untyped).iter().any(|e| e.contains("no `type` const")));
}

#[test]
fn the_planned_mark_accepts_only_its_one_value() {
    let mut errors = Vec::new();
    assert!(planned(&json!({PLANNED: "planned"}), "x", &mut errors));
    assert!(!planned(&json!({}), "x", &mut errors));
    assert!(errors.is_empty());
    assert!(!planned(&json!({PLANNED: "later"}), "x", &mut errors));
    assert_eq!(errors.len(), 1);
}
