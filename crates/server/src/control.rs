//! `GET /api/v1/agent/control`: the server's side of the control channel (design 5.2, 13.3, 14.1;
//! ADR-466, ADR-468; `agentControl` in `api/asyncapi.yaml`).
//!
//! The route is judged in this order, and the first step that applies answers: the request guard
//! (before routing), the method (405), the bearer token (401 with `WWW-Authenticate: Bearer`),
//! the WebSocket request (400), the subprotocol (400), then the upgrade (101). Every refusal is
//! the server's problem document. The token is judged here and nowhere else: it is not consumed
//! by the upgrade, not bound to the connection and not looked at again.
//!
//! Each accepted socket runs as one task that [`Hub`] spawns and owns. The task stamps the
//! agent's `last_seen_at` (ADR-468 item 11), registers with the hub, writes `hello`, reads complete
//! messages, echoes `heartbeat`, hands every other valid message to the hub's inbox, writes the job
//! commands the hub queued, and ends by the close table of the asyncapi file. All writes come
//! from that one task, so the socket is not split.
//!
//! Never logged: a token, or the text of a message.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use shared_proto::{
    AgentId, AgentMessage, CLOSE_CODE_BINARY, CLOSE_CODE_GOING_AWAY, CLOSE_CODE_IDLE,
    CLOSE_CODE_PROTOCOL, CLOSE_CODE_REVOKED, CLOSE_CODE_TOO_BIG, CONTROL_SUBPROTOCOL, ControlError,
    DEFAULT_HEARTBEAT_SECS, HEARTBEAT_MISSED_LIMIT, MAX_CONTROL_MESSAGE_LEN, ServerMessage,
    close_reason,
};
use sqlx::PgPool;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use crate::health::may_log;
use crate::hub::{ConnectionId, Hub, Inbound, Registration, Stopping};
use crate::problem::Problem;
use crate::session::{Sessions, now};

/// The route's path.
pub const PATH: &str = "/api/v1/agent/control";

/// How long one write to the agent may take before the connection is given up.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server waits for the peer's answer to its Close frame, and, after a read error
/// (when no further read is possible), how long it keeps the connection open so that the Close
/// frame is not lost to a reset.
pub(crate) const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// How long the stamp of `last_seen_at` may take, connection and statement together.
pub(crate) const STAMP_TIMEOUT: Duration = Duration::from_secs(5);
/// The refusal at the upgrade, and each failure of the stamp, is logged at most this often.
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The echo of a heartbeat: the text `ServerMessage::Heartbeat` encodes to, written without
/// building it each time (a test keeps the two equal).
const HEARTBEAT_ECHO: &str = r#"{"type":"heartbeat"}"#;

type Gate = Mutex<Option<std::time::Instant>>;

/// The log gates of the connections' own events, one each so that a flood of one cannot silence
/// another.
#[derive(Default)]
struct Gates {
    /// A failed stamp at connect.
    connect: Gate,
    /// A failed stamp at disconnect.
    disconnect: Gate,
    /// A message dropped because nobody takes the inbox.
    dropped: Gate,
}

/// The state of the route: the pool, the token table, the hub, the log gate of the 401 and the
/// gates of the connections.
#[derive(Clone)]
pub struct Control {
    pool: PgPool,
    sessions: Sessions,
    hub: Hub,
    gate: Arc<Gate>,
    gates: Arc<Gates>,
}

impl Control {
    pub fn new(pool: PgPool, sessions: Sessions, hub: Hub) -> Control {
        Control {
            pool,
            sessions,
            hub,
            gate: Arc::new(Mutex::new(None)),
            gates: Arc::default(),
        }
    }
}

/// The token text of an `Authorization` header, or `None` for anything but exactly one header of
/// the form scheme `Bearer` (compared ignoring ASCII case), one or more spaces and a non-empty
/// token. Whether the token is one is [`Sessions::authenticate`]'s question.
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, rest) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let token = rest.trim_start_matches(' ');
    (!token.is_empty()).then_some(token)
}

fn unauthorized() -> Response {
    let mut response =
        Problem::new(StatusCode::UNAUTHORIZED, "A session token is required").into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// The handler. It takes the whole request so that the token is judged before axum's WebSocket
/// extractor, whose own rejection (400) would otherwise answer a caller without a token.
pub async fn upgrade(State(control): State<Control>, request: Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    // The route takes every method, so that this 405 is the problem document; HEAD is refused too.
    if parts.method != Method::GET {
        let mut response =
            Problem::new(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET"));
        return response;
    }
    let agent = bearer_token(&parts.headers).and_then(|token| control.sessions.authenticate(token));
    let Some(agent) = agent else {
        if may_log(&control.gate, now(), LOG_EVERY) {
            tracing::debug!("control channel refused: no live session token");
        }
        return unauthorized();
    };
    let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return Problem::new(StatusCode::BAD_REQUEST, "A WebSocket upgrade is required")
            .into_response();
    };
    let ws = ws.protocols([CONTROL_SUBPROTOCOL]);
    if ws.selected_protocol().is_none() {
        return Problem::new(
            StatusCode::BAD_REQUEST,
            "The subprotocol ngr-control.v1 is required",
        )
        .into_response();
    }
    let hub = control.hub.clone();
    // Both limits equal the message limit: the stack refuses an oversized frame on its header,
    // before it reads the payload.
    ws.max_message_size(MAX_CONTROL_MESSAGE_LEN)
        .max_frame_size(MAX_CONTROL_MESSAGE_LEN)
        .on_failed_upgrade(|error| tracing::debug!(%error, "control channel upgrade failed"))
        .on_upgrade(move |socket| async move {
            // axum runs this callback on a task it detaches; the session runs on the hub's.
            hub.adopt(session(control, agent, socket));
        })
}

// ---------------------------------------------------------------------------------------------
// One connection

/// How a connection ends.
#[derive(Debug, PartialEq, Eq)]
enum Finish {
    /// Send Close with this code, then read until the peer has answered.
    Close(u16),
    /// Send Close, then wait without reading: after a read error the stack allows no read.
    CloseThenWait(u16),
    /// The peer sent Close: one more read lets the stack write its reply.
    PeerClosed,
    /// End without a frame: the transport is gone, a write failed, or the hub let go of it.
    Drop(&'static str),
}

impl Finish {
    /// The fixed word the log and the close reason use; never anything the peer sent.
    fn word(&self) -> &'static str {
        match self {
            Finish::Close(code) | Finish::CloseThenWait(code) => close_reason(*code),
            Finish::PeerClosed => "peer closed",
            Finish::Drop(word) => word,
        }
    }
}

/// The close for a message that did not decode: the code is the shared table's (ADR-471).
fn finish_for(error: &ControlError) -> Finish {
    Finish::Close(error.close_code())
}

/// The end for a read error. A message over the limit is 1009; invalid UTF-8 and a framing
/// violation are 1002, except a connection reset without a closing handshake, which is a
/// transport fact and gets no frame. Whatever else it is (I/O, a closed connection) gets none.
fn finish_for_read_error(error: axum::Error) -> Finish {
    use tungstenite::Error;
    use tungstenite::error::ProtocolError;
    match error.into_inner().downcast::<Error>().map(|e| *e) {
        Ok(Error::Capacity(_)) => Finish::CloseThenWait(CLOSE_CODE_TOO_BIG),
        Ok(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)) => Finish::Drop("reset"),
        Ok(Error::Utf8(_) | Error::Protocol(_)) => Finish::CloseThenWait(CLOSE_CODE_PROTOCOL),
        _ => Finish::Drop("transport"),
    }
}

/// Ends the connection's registration when dropped, so that a task that panics or is aborted
/// leaves no entry behind. [`Hub::leave`] is idempotent.
struct Leave {
    hub: Hub,
    agent: AgentId,
    id: ConnectionId,
}

impl Drop for Leave {
    fn drop(&mut self) {
        self.hub.leave(&self.agent, self.id);
    }
}

/// Stamps the agent's `last_seen_at` and says whether its row is revoked (`None`: there is no
/// row), within [`STAMP_TIMEOUT`]. The error is a fixed word, never the database's text. The
/// transaction is dropped, and so rolled back, when the time is up.
async fn stamp(pool: &PgPool, agent: &AgentId) -> Result<Option<bool>, &'static str> {
    match tokio::time::timeout(STAMP_TIMEOUT, crate::db::stamp_agent(pool, &agent.0)).await {
        Ok(Ok(revoked)) => Ok(revoked),
        Ok(Err(error)) => Err(match crate::db::failure(&error) {
            crate::db::Failure::Unavailable(_) => "the database is unavailable",
            crate::db::Failure::Internal(_) => "the statement failed",
        }),
        Err(_) => Err("the database did not answer in time"),
    }
}

/// The whole life of one connection.
async fn session(control: Control, agent: AgentId, mut socket: WebSocket) {
    // The stamp comes first, so that a client that has read `hello` knows it is done, and so
    // that a revoked agent displaces nobody (ADR-468 item 11). A stop that begins meanwhile is
    // not raced: the registration below refuses it.
    match stamp(&control.pool, &agent).await {
        Ok(Some(false)) => {}
        Ok(_) => {
            control.sessions.revoke_agent(&agent);
            tracing::warn!(agent_id = %agent.0, "control channel refused: the agent is revoked or unknown");
            if send_close(&mut socket, CLOSE_CODE_REVOKED).await {
                drain(&mut socket).await;
            }
            return;
        }
        Err(what) => {
            if may_log(&control.gates.connect, now(), LOG_EVERY) {
                tracing::warn!(agent_id = %agent.0, what, "control channel: last_seen_at was not written at connect");
            }
        }
    }
    let mut registration = match control.hub.register(&agent) {
        Ok(registration) => registration,
        Err(Stopping) => {
            // The stop began after the hub adopted this session and before it registered.
            if send_close(&mut socket, CLOSE_CODE_GOING_AWAY).await {
                drain(&mut socket).await;
            }
            return;
        }
    };
    let id = registration.id;
    let leave = Leave {
        hub: control.hub.clone(),
        agent,
        id,
    };
    tracing::info!(agent_id = %leave.agent.0, connection = %id, "control channel connected");
    let end = run(&mut socket, &mut registration, &control, &leave.agent).await;
    // The hub stops routing to this connection before anything is written to it. The guard's
    // own drop leaves again, which removes nothing. Only the agent's current connection is a
    // disconnect of the agent: a replaced one is not.
    let current = leave.hub.leave(&leave.agent, id);
    finish(
        &mut socket,
        &end,
        &mut registration.stopping,
        Disconnect {
            control: &control,
            agent: &leave.agent,
            current,
        },
    )
    .await;
    tracing::info!(
        agent_id = %leave.agent.0,
        connection = %id,
        end = end.word(),
        "control channel ended"
    );
}

/// The connection's loop: `hello`, then messages until something ends it.
async fn run(
    socket: &mut WebSocket,
    registration: &mut Registration,
    control: &Control,
    agent: &AgentId,
) -> Finish {
    let Registration {
        id,
        close,
        stopping,
        outbound,
    } = registration;
    let hello = ServerMessage::Hello {
        heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
    };
    let Ok(hello) = hello.encode() else {
        return Finish::Drop("encode");
    };
    if !write(socket, Message::Text(hello.into())).await {
        return Finish::Drop("write");
    }
    let idle = Duration::from_secs(u64::from(DEFAULT_HEARTBEAT_SECS * HEARTBEAT_MISSED_LIMIT));
    // The idle clock starts once `hello` is written, and restarts after every complete message,
    // ping or pong has been handled. A command written to the agent does not restart it.
    let mut deadline = Instant::now() + idle;
    loop {
        tokio::select! {
            instruction = &mut *close => {
                return match instruction {
                    Ok(code) => Finish::Close(code),
                    Err(_) => Finish::Drop("unregistered"),
                };
            }
            () = stopped(stopping) => return Finish::Close(CLOSE_CODE_GOING_AWAY),
            // `None` means the hub let go of the queue (the entry was replaced): the close
            // instruction is then on its way, so this arm is simply not taken.
            Some(frame) = outbound.recv() => {
                // A frame queued before an instruction or a stop is not written after it.
                if let Ok(code) = close.try_recv() {
                    return Finish::Close(code);
                }
                if *stopping.borrow() {
                    return Finish::Close(CLOSE_CODE_GOING_AWAY);
                }
                if !write(socket, Message::Text(frame.into())).await {
                    return Finish::Drop("write");
                }
            }
            received = socket.recv() => {
                let message = match received {
                    None => return Finish::Drop("closed"),
                    Some(Err(error)) => return finish_for_read_error(error),
                    Some(Ok(message)) => message,
                };
                match message {
                    Message::Text(text) => match AgentMessage::decode(text.as_str()) {
                        Ok(AgentMessage::Heartbeat) => {
                            if !write(socket, Message::Text(HEARTBEAT_ECHO.into())).await {
                                return Finish::Drop("write");
                            }
                        }
                        Ok(message) => {
                            if let Some(end) =
                                deliver(control, agent, *id, message, close, stopping).await
                            {
                                return end;
                            }
                        }
                        Err(error) => return finish_for(&error),
                    },
                    Message::Binary(_) => return Finish::Close(CLOSE_CODE_BINARY),
                    // The stack answers a ping itself; both count as a sign of life.
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return Finish::PeerClosed,
                }
                deadline = Instant::now() + idle;
            }
            () = tokio::time::sleep_until(deadline) => return Finish::Close(CLOSE_CODE_IDLE),
        }
    }
}

/// Hands a message to the hub's inbox. With nobody holding the inbox the message is dropped. With
/// a full inbox the task holds the message and waits, reading nothing, while it still watches its
/// close instruction and the stop; the wait is not idle time, because the caller restarts the
/// idle clock after this returns. `Some` ends the connection.
async fn deliver(
    control: &Control,
    agent: &AgentId,
    connection: ConnectionId,
    message: AgentMessage,
    close: &mut oneshot::Receiver<u16>,
    stopping: &mut watch::Receiver<bool>,
) -> Option<Finish> {
    // Asked per message, so that an inbox taken after the connection registered is used.
    let Some(inbox) = control.hub.inbox_sender() else {
        if may_log(&control.gates.dropped, now(), LOG_EVERY) {
            tracing::debug!("a control message was read and nobody takes the inbox; dropped");
        }
        return None;
    };
    let inbound = Inbound {
        agent: agent.clone(),
        connection,
        message,
    };
    let inbound = match inbox.try_send(inbound) {
        Ok(()) => return None,
        Err(TrySendError::Closed(_)) => {
            if may_log(&control.gates.dropped, now(), LOG_EVERY) {
                tracing::debug!("a control message was read and the inbox is gone; dropped");
            }
            return None;
        }
        Err(TrySendError::Full(inbound)) => inbound,
    };
    tokio::select! {
        sent = inbox.send(inbound) => {
            if sent.is_err() && may_log(&control.gates.dropped, now(), LOG_EVERY) {
                tracing::debug!("a control message was held and the inbox went away; dropped");
            }
            None
        }
        instruction = &mut *close => Some(match instruction {
            Ok(code) => Finish::Close(code),
            Err(_) => Finish::Drop("unregistered"),
        }),
        () = stopped(stopping) => Some(Finish::Close(CLOSE_CODE_GOING_AWAY)),
    }
}

/// Resolves when the stop has begun (or the hub is gone).
async fn stopped(stopping: &mut watch::Receiver<bool>) {
    let _ = stopping.wait_for(|stop| *stop).await;
}

/// Writes one message within [`WRITE_TIMEOUT`]; false when it failed or took too long.
async fn write(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn send_close(socket: &mut WebSocket, code: u16) -> bool {
    let frame = CloseFrame {
        code,
        reason: close_reason(code).into(),
    };
    write(socket, Message::Close(Some(frame))).await
}

/// Reads and discards until the peer has answered a Close or [`CLOSE_WAIT`] has passed, so that
/// the peer's reply is read and TCP ends without a reset.
async fn drain(socket: &mut WebSocket) {
    let deadline = Instant::now() + CLOSE_WAIT;
    while let Ok(Some(Ok(_))) = tokio::time::timeout_at(deadline, socket.recv()).await {}
}

/// What the end of a connection owes the database: the stamp of the agent's disconnect, written
/// when the connection was the agent's current one (ADR-468 item 11).
struct Disconnect<'a> {
    control: &'a Control,
    agent: &'a AgentId,
    current: bool,
}

impl Disconnect<'_> {
    /// Stamps the row and ignores the answer; a failure is logged through its gate.
    async fn write(&self) {
        if !self.current {
            return;
        }
        if let Err(what) = stamp(&self.control.pool, self.agent).await
            && may_log(&self.control.gates.disconnect, now(), LOG_EVERY)
        {
            tracing::warn!(agent_id = %self.agent.0, what, "control channel: last_seen_at was not written at disconnect");
        }
    }
}

/// What follows the end of the loop. The stamp of the disconnect comes after the Close frame (or
/// after the reply to the peer's own Close) and before the wait for the peer.
async fn finish(
    socket: &mut WebSocket,
    end: &Finish,
    stopping: &mut watch::Receiver<bool>,
    disconnect: Disconnect<'_>,
) {
    match end {
        Finish::Close(code) => {
            let sent = send_close(socket, *code).await;
            disconnect.write().await;
            if sent {
                drain(socket).await;
            }
        }
        Finish::CloseThenWait(code) => {
            let sent = send_close(socket, *code).await;
            disconnect.write().await;
            if sent {
                // No read is possible after a read error. Dropping at once with input unread
                // can reset the connection and lose the Close at the peer, so wait; a stop ends
                // the wait.
                tokio::select! {
                    () = tokio::time::sleep(CLOSE_WAIT) => {}
                    () = stopped(stopping) => {}
                }
            }
        }
        Finish::PeerClosed => {
            let _ = tokio::time::timeout(CLOSE_WAIT, socket.recv()).await;
            disconnect.write().await;
        }
        Finish::Drop(_) => disconnect.write().await,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderName;

    use super::*;

    fn headers(values: &[&[u8]]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(
                HeaderName::from_static("authorization"),
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        map
    }

    fn token_of(values: &[&[u8]]) -> Option<String> {
        bearer_token(&headers(values)).map(str::to_owned)
    }

    #[test]
    fn the_authorization_header_is_one_bearer_scheme_and_a_token() {
        let accepted: [(&[u8], &str); 6] = [
            (b"Bearer abc", "abc"),
            (b"bearer abc", "abc"),
            (b"BEARER abc", "abc"),
            (b"Bearer   abc", "abc"),
            // The token text goes to the table as it is: a trailing space is part of it.
            (b"Bearer abc ", "abc "),
            // A second word is part of the token text too, and fails the table's check.
            (b"Bearer a b", "a b"),
        ];
        for (value, token) in accepted {
            assert_eq!(token_of(&[value]).as_deref(), Some(token), "{value:?}");
        }
        let refused: [&[&[u8]]; 10] = [
            &[],
            &[b"Basic abc"],
            &[b"Bearer"],
            &[b"Bearer "],
            &[b"Bearer    "],
            &[b"Bearer\tabc"],
            &[b"Bearerabc"],
            &[b"Bearer \xE9"],
            &[b"Bearer abc", b"Bearer abc"],
            &[b"Bearer abc", b"Bearer def"],
        ];
        for values in refused {
            assert_eq!(token_of(values), None, "{values:?}");
        }
    }

    #[test]
    fn the_heartbeat_echo_is_what_the_message_encodes_to() {
        assert_eq!(ServerMessage::Heartbeat.encode().unwrap(), HEARTBEAT_ECHO);
    }

    #[test]
    fn a_message_that_does_not_decode_closes_with_1002_or_1009() {
        for text in [
            "not json",
            "[]",
            "{}",
            r#"{"type":"nope"}"#,
            r#"{"type":"hello","heartbeatSecs":20}"#,
            r#"{"type":"heartbeat","extra":1}"#,
            r#"{"type":"commandAck"}"#,
            r#"{"type":"commandAck","commandId":1}"#,
        ] {
            let error = AgentMessage::decode(text).unwrap_err();
            assert_eq!(finish_for(&error), Finish::Close(1002), "{text}");
        }
        let long = format!(
            r#"{{"type":"commandAck","commandId":"{}"}}"#,
            "x".repeat(MAX_CONTROL_MESSAGE_LEN)
        );
        let error = AgentMessage::decode(&long).unwrap_err();
        assert_eq!(finish_for(&error), Finish::Close(1009));
    }

    #[test]
    fn a_read_error_is_classified_by_what_the_stack_says() {
        use tungstenite::Error;
        use tungstenite::error::{CapacityError, ProtocolError};
        let of = |error: Error| finish_for_read_error(axum::Error::new(error));
        assert_eq!(
            of(Error::Capacity(CapacityError::MessageTooLong {
                size: 2,
                max_size: 1
            })),
            Finish::CloseThenWait(1009)
        );
        assert_eq!(
            of(Error::Utf8("bad".to_owned())),
            Finish::CloseThenWait(1002)
        );
        assert_eq!(
            of(Error::Protocol(ProtocolError::UnmaskedFrameFromClient)),
            Finish::CloseThenWait(1002)
        );
        assert_eq!(
            of(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake)),
            Finish::Drop("reset")
        );
        assert_eq!(of(Error::ConnectionClosed), Finish::Drop("transport"));
        assert_eq!(of(Error::AlreadyClosed), Finish::Drop("transport"));
        assert_eq!(
            of(Error::Io(std::io::Error::other("gone"))),
            Finish::Drop("transport")
        );
        // An error that is not the stack's is no evidence of anything either.
        assert_eq!(
            finish_for_read_error(axum::Error::new(std::io::Error::other("x"))),
            Finish::Drop("transport")
        );
    }
}
