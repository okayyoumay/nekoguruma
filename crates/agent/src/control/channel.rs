//! The upgrade and the connection's loop (design 5.2, 14.1, ADR-466, ADR-471; `agentControl` in
//! `api/asyncapi.yaml`).
//!
//! [`upgrade`] opens the WebSocket with the token and the subprotocol. [`serve`] then runs one
//! connection: it waits for `hello`, sends `heartbeat` at the interval `hello` names, reads what
//! the server sends and closes the connection when something is wrong. The loop is driven only by
//! `tokio::time`, with one writer (this task), and its select is biased while connected: stop,
//! the idle deadline, the heartbeat tick, then the read, so a server that is silent since `hello`
//! gets heartbeats at one and two intervals and the idle close at three.
//!
//! What ends a connection is an [`End`]: the close to send (or none), how the loop backs off and
//! what the log says. Job commands are read and counted and answered by no one yet. Never logged:
//! the token, a message's text, a `commandId`, a close reason the server sent.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use shared_proto::{
    CLOSE_CODE_BINARY, CLOSE_CODE_GOING_AWAY, CLOSE_CODE_IDLE, CLOSE_CODE_PROTOCOL,
    CLOSE_CODE_REPLACED, CLOSE_CODE_REVOKED, CLOSE_CODE_TOO_BIG, CONTROL_SUBPROTOCOL,
    HEARTBEAT_MISSED_LIMIT, MAX_CONTROL_MESSAGE_LEN, ServerMessage, close_reason,
};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, ClientRequestBuilder, Message, http};

use super::backoff::lived;
use super::dial::{self, ConnectError, Stream};
use super::exchange::Token;
use super::{Fail, Outcome, Phase, SessionState, Stop, Wiring};
use crate::server_http;

/// The upgrade route.
pub(super) const CONTROL_PATH: &str = "/api/v1/agent/control";

/// How long `hello` may take after the 101.
pub(super) const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How long one write may take.
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the agent waits for the peer's answer to its Close, and, after a read error (when no
/// further read is possible), how long it keeps the connection open so the Close is not lost to
/// a reset.
pub(super) const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// The text of the agent's heartbeat (a test keeps it equal to what the message encodes to).
pub(super) const HEARTBEAT: &str = r#"{"type":"heartbeat"}"#;

pub(super) type Socket = WebSocketStream<Stream>;

// ---------------------------------------------------------------------------------------------
// The upgrade

/// Why the upgrade gave no connection: the log's word for it and the status when there is one.
pub(super) struct UpgradeFail {
    pub(super) class: &'static str,
    pub(super) status: Option<u16>,
}

fn fail(class: &'static str, status: Option<u16>) -> UpgradeFail {
    UpgradeFail { class, status }
}

/// Opens the WebSocket (D). The request has `Host` from the record, the bearer token and the
/// subprotocol, and no `Origin` and no extensions; the bounds are 5 seconds to connect and 30 in
/// all.
pub(super) async fn upgrade(w: &Wiring, token: &Token) -> Result<Socket, UpgradeFail> {
    let (connect_by, deadline) =
        server_http::deadlines(Instant::now(), server_http::Timeouts::default());
    let stream = match dial::connect(&*w.dial, connect_by).await {
        Ok(stream) => stream,
        Err(ConnectError::Timeout | ConnectError::Failed) => {
            return Err(fail("upgrade-transport", None));
        }
    };
    let uri: http::Uri = format!("ws://{}{CONTROL_PATH}", w.host)
        .parse()
        .map_err(|_| fail("upgrade-request", None))?;
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {}", token.as_str()))
        .with_sub_protocol(CONTROL_SUBPROTOCOL);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_CONTROL_MESSAGE_LEN))
        .max_frame_size(Some(MAX_CONTROL_MESSAGE_LEN));
    let handshake = tokio_tungstenite::client_async_with_config(request, stream, Some(config));
    match tokio::time::timeout_at(deadline, handshake).await {
        Err(_) => Err(fail("upgrade-transport", None)),
        Ok(Ok((socket, _response))) => Ok(socket),
        Ok(Err(error)) => Err(upgrade_error(&error)),
    }
}

/// D2 and D4 to D8: what a failed handshake is.
fn upgrade_error(error: &tungstenite::Error) -> UpgradeFail {
    use tungstenite::Error;
    match error {
        Error::Http(response) => {
            let status = response.status().as_u16();
            let class = match status {
                400 => "upgrade-bad-request",
                401 => "upgrade-unauthorized",
                300..=399 => "upgrade-redirect",
                _ => "upgrade-status",
            };
            fail(class, Some(status))
        }
        Error::Io(_) | Error::ConnectionClosed | Error::AlreadyClosed => {
            fail("upgrade-transport", None)
        }
        _ => fail("upgrade-handshake", None),
    }
}

// ---------------------------------------------------------------------------------------------
// How a connection ends

/// What the agent writes and reads to end the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// Send Close with the code, read and discard until the peer's Close or [`CLOSE_WAIT`].
    Close(u16),
    /// Send Close, then wait [`CLOSE_WAIT`] without reading: after a stack read error no read is
    /// possible.
    Linger(u16),
    /// The peer's Close was read: one more read of at most [`CLOSE_WAIT`] lets the stack answer.
    Reply,
    /// No frame: the transport is gone or a write failed.
    Drop,
    /// The agent is stopping: Close 1001, and the task ends.
    GoAway,
}

/// What the log says about the end, besides the one info line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Warn {
    None,
    /// One warning per failure class since the last `hello`.
    Class(&'static str),
    /// One warning per refusal (the refusal flag).
    Refusal(&'static str),
}

/// The decision that ends a connection.
#[derive(Debug, Clone, Copy)]
pub(super) struct End {
    action: Action,
    how: Fail,
    /// Whether the connection lived (`hello` to its last frame at least 60 seconds): the delay
    /// resets, unless `how` is the cap.
    lived: bool,
    warn: Warn,
    /// The word the end's log line gives.
    word: &'static str,
    /// The peer's close code, when the end is the peer's Close.
    peer_code: Option<u16>,
    /// Whether the agent's own stop ended the connection: the task then ends, with no wait.
    stopped: bool,
    decided_at: Instant,
}

impl End {
    fn new(action: Action, word: &'static str, warn: Warn, lived: bool) -> End {
        End {
            action,
            how: Fail::Ord,
            lived,
            warn,
            word,
            peer_code: None,
            stopped: false,
            decided_at: Instant::now(),
        }
    }

    fn stop() -> End {
        End {
            stopped: true,
            ..End::new(Action::GoAway, "stopped", Warn::None, false)
        }
    }

    /// A stop that came while a write was stalled: the Close would stall behind it, so the
    /// connection is dropped.
    fn stop_dropped() -> End {
        End {
            stopped: true,
            ..End::new(Action::Drop, "stopped", Warn::None, false)
        }
    }

    /// A close the agent decides on: the code goes to the peer and to the log.
    fn close(code: u16, warn_class: &'static str, lived: bool) -> End {
        End::new(
            Action::Close(code),
            close_reason(code),
            Warn::Class(warn_class),
            lived,
        )
    }

    fn dropped(word: &'static str, lived: bool) -> End {
        End::new(Action::Drop, word, Warn::Class("transport"), lived)
    }
}

/// How a stack read error ends the connection (the server's classification, mirrored): a message
/// over the limit is 1009; invalid UTF-8 and a framing violation are 1002, except a reset without
/// a closing handshake, which is a transport fact and gets no frame. Whatever else it is gets
/// none either.
pub(super) fn classify_read_error(error: &tungstenite::Error) -> Action {
    use tungstenite::Error;
    use tungstenite::error::ProtocolError;
    match error {
        Error::Capacity(_) => Action::Linger(CLOSE_CODE_TOO_BIG),
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => Action::Drop,
        Error::Utf8(_) | Error::Protocol(_) => Action::Linger(CLOSE_CODE_PROTOCOL),
        _ => Action::Drop,
    }
}

fn read_error(error: &tungstenite::Error, lived: bool) -> End {
    match classify_read_error(error) {
        Action::Linger(code) => End::new(
            Action::Linger(code),
            close_reason(code),
            Warn::Class("server-frame"),
            lived,
        ),
        _ => End::dropped("transport", lived),
    }
}

/// G: the server sent Close. The code is logged, the reason text never. `lived` is whether the
/// connection lived, which is false before `hello`.
fn peer_close(frame: Option<CloseFrame>, lived: bool) -> End {
    let code = frame.map(|frame| u16::from(frame.code));
    let mut end = End::new(Action::Reply, "peer closed", Warn::None, lived);
    end.peer_code = code;
    match code {
        Some(CLOSE_CODE_REPLACED) => {
            end.how = Fail::Cap;
            end.warn = Warn::Class("replaced");
        }
        Some(CLOSE_CODE_REVOKED) => {
            end.how = Fail::Cap;
            end.warn = Warn::Refusal("revoked");
        }
        Some(CLOSE_CODE_PROTOCOL | CLOSE_CODE_BINARY | CLOSE_CODE_TOO_BIG) => {
            end.warn = Warn::Class("server-closed-protocol");
        }
        Some(CLOSE_CODE_IDLE) => end.warn = Warn::Class("server-closed-idle"),
        _ => {}
    }
    end
}

// ---------------------------------------------------------------------------------------------
// Writes and closes

/// Writes one text message within [`WRITE_TIMEOUT`]; false when it failed or took too long.
async fn write_text(socket: &mut Socket, text: &'static str) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(text.into()))).await,
        Ok(Ok(()))
    )
}

async fn send_close(socket: &mut Socket, code: u16) -> bool {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: close_reason(code).into(),
    };
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Close(Some(frame)))).await,
        Ok(Ok(()))
    )
}

/// Reads and discards until the peer's Close or [`CLOSE_WAIT`], so that the peer's reply is read
/// and TCP ends without a reset.
async fn drain(socket: &mut Socket) {
    let deadline = Instant::now() + CLOSE_WAIT;
    while let Ok(Some(Ok(message))) = tokio::time::timeout_at(deadline, socket.next()).await {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
}

/// Carries out the end's close. A stop interrupts it, except when a stop decided the end (`stop`
/// is `None`).
async fn close_out(socket: &mut Socket, action: Action, stop: Option<&Stop>) {
    let run = async {
        match action {
            Action::Close(code) => {
                if send_close(socket, code).await {
                    drain(socket).await;
                }
            }
            Action::GoAway => {
                if send_close(socket, CLOSE_CODE_GOING_AWAY).await {
                    drain(socket).await;
                }
            }
            Action::Linger(code) => {
                if send_close(socket, code).await {
                    tokio::time::sleep(CLOSE_WAIT).await;
                }
            }
            Action::Reply => {
                let _ = tokio::time::timeout(CLOSE_WAIT, socket.next()).await;
            }
            Action::Drop => {}
        }
    };
    match stop {
        Some(stop) => {
            tokio::select! {
                () = stop.requested() => {}
                () = run => {}
            }
        }
        None => run.await,
    }
}

/// Carries out `end`, writes its log lines and returns what the loop does next.
async fn finish(socket: &mut Socket, end: End, state: &mut SessionState<'_>) -> Outcome {
    state.status.phase(Phase::Closing);
    let stop = (!end.stopped).then_some(state.stop);
    close_out(socket, end.action, stop).await;
    match end.warn {
        Warn::None => {}
        Warn::Class(class) => state.warnings.failure(class, end.peer_code),
        Warn::Refusal(class) => state.warnings.refusal(class, end.peer_code.unwrap_or(0)),
    }
    tracing::info!(end = end.word, code = ?end.peer_code, "control channel ended");
    // A stop that came while the end was carried out (a write that failed as the stop came, a
    // close the stop cut short) ends the task as a stop does: no wait comes first.
    if end.stopped || state.stop.is_set() {
        return Outcome::Stopped;
    }
    Outcome::Failed {
        decided_at: end.decided_at,
        how: end.how,
        lived: end.lived,
    }
}

// ---------------------------------------------------------------------------------------------
// The connection

/// Runs one connection to its end.
pub(super) async fn serve(mut socket: Socket, state: &mut SessionState<'_>) -> Outcome {
    let end = match await_hello(&mut socket, state.stop).await {
        Err(end) => end,
        Ok(secs) => {
            state.warnings.hello();
            tracing::info!(heartbeat_secs = secs, "control channel connected");
            state.status.connected(secs);
            connected(&mut socket, secs, state).await
        }
    };
    finish(&mut socket, end, state).await
}

/// E: nothing counts before `hello`, and nothing but a valid `hello` is handled.
async fn await_hello(socket: &mut Socket, stop: &Stop) -> Result<u32, End> {
    let deadline = Instant::now() + HELLO_WAIT;
    loop {
        tokio::select! {
            biased;
            () = stop.requested() => return Err(End::stop()),
            () = tokio::time::sleep_until(deadline) => {
                return Err(End::close(CLOSE_CODE_PROTOCOL, "no-hello", false));
            }
            received = socket.next() => match received {
                None => return Err(End::dropped("eof", false)),
                Some(Err(error)) => return Err(read_error(&error, false)),
                Some(Ok(Message::Text(text))) => {
                    return match ServerMessage::decode(text.as_str()) {
                        Ok(ServerMessage::Hello { heartbeat_secs }) => Ok(heartbeat_secs),
                        Ok(_) => Err(End::close(CLOSE_CODE_PROTOCOL, "before-hello", false)),
                        Err(error) => Err(End::close(error.close_code(), "server-message", false)),
                    };
                }
                Some(Ok(Message::Binary(_))) => {
                    return Err(End::close(CLOSE_CODE_BINARY, "server-binary", false));
                }
                // The stack answers a ping itself; neither extends the deadline.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => return Err(peer_close(frame, false)),
            }
        }
    }
}

/// F: after `hello`. Returns the decision that ends the connection.
async fn connected(socket: &mut Socket, secs: u32, state: &mut SessionState<'_>) -> End {
    let interval = Duration::from_secs(u64::from(secs));
    let idle = interval * HEARTBEAT_MISSED_LIMIT;
    let hello_at = Instant::now();
    let mut last_frame_at = hello_at;
    // Ticks fall at `hello_at + k * interval`.
    let mut tick = 1u32;
    loop {
        let lived_now = lived(hello_at, last_frame_at);
        tokio::select! {
            biased;
            () = state.stop.requested() => return End::stop(),
            () = tokio::time::sleep_until(last_frame_at + idle) => {
                return End::close(CLOSE_CODE_IDLE, "idle", lived_now);
            }
            () = tokio::time::sleep_until(hello_at + interval * tick) => {
                // A stop does not wait for a stalled write: the transport that stalls it would
                // stall the Close as well, so the connection is dropped (H5). The write is
                // polled first, so one that completes at once is followed by Close 1001.
                let written = tokio::select! {
                    biased;
                    written = write_text(socket, HEARTBEAT) => Some(written),
                    () = state.stop.requested() => None,
                };
                match written {
                    None => return End::stop_dropped(),
                    Some(false) => return End::dropped("write", lived_now),
                    Some(true) => {}
                }
                tick += 1;
                while hello_at + interval * tick <= Instant::now() {
                    tick += 1;
                }
            }
            received = socket.next() => match received {
                None => return End::dropped("eof", lived_now),
                Some(Err(error)) => return read_error(&error, lived_now),
                Some(Ok(Message::Text(text))) => match ServerMessage::decode(text.as_str()) {
                    Ok(ServerMessage::Heartbeat) => last_frame_at = Instant::now(),
                    Ok(ServerMessage::Hello { .. }) => {
                        return End::close(CLOSE_CODE_PROTOCOL, "second-hello", lived_now);
                    }
                    Ok(ServerMessage::JobCommand { .. }) => {
                        // Read and counted; answered by no one yet (the job-command work).
                        state.status.command_received();
                        last_frame_at = Instant::now();
                    }
                    Ok(
                        ServerMessage::CancelCommand { .. }
                        | ServerMessage::HmiResponse { .. }
                        | ServerMessage::SeedKeyResponse { .. }
                        | ServerMessage::EventAck { .. },
                    ) => {
                        tracing::debug!("a control message was read and not handled");
                        last_frame_at = Instant::now();
                    }
                    Err(error) => {
                        return End::close(error.close_code(), "server-message", lived_now);
                    }
                },
                Some(Ok(Message::Binary(_))) => {
                    return End::close(CLOSE_CODE_BINARY, "server-binary", lived_now);
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => last_frame_at = Instant::now(),
                Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(frame))) => return peer_close(frame, lived_now),
            }
        }
    }
}
