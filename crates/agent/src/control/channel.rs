//! The upgrade and the connection's loop (design 5.2, 14.1, ADR-466, ADR-471; `agentControl` in
//! `api/asyncapi.yaml`).
//!
//! [`upgrade`] opens the WebSocket with the token and the subprotocol. [`serve`] then runs one
//! connection: it waits for `hello`, sends `heartbeat` at the interval `hello` names, reads what
//! the server sends and closes the connection when something is wrong. The loop is driven only by
//! `tokio::time`, with one writer (this task), and its select is biased while connected: stop,
//! the idle deadline, the heartbeat tick, the read, then the outbox, so a server that is silent
//! since `hello` gets heartbeats at one and two intervals and the idle close at three, and a long
//! backlog of events delays neither a heartbeat nor a read.
//!
//! The outbox arm writes the next pending event ([`super::events`], ADR-478); a sent event is not
//! a received frame, so it does not move the idle deadline. `eventAck` is applied to the outbox
//! and counts as a received frame; one above the highest number assigned closes with 1002.
//!
//! What ends a connection is an [`End`]: the close to send (or none), how the loop backs off and
//! what the log says. A job command goes to the connection's [`Gate`], and what follows is
//! [`job_command`]'s order (design 5.3, ADR-461 item 9, ADR-489): verify, raise the highest issue
//! time, the operator check, the duplicate check against the received-jobs ledger, the deadline
//! check, sync the mark, record the job in the ledger, hand it to the [`Intake`], append the
//! `received` event, answer `commandAck`. A verified instruction for a job the ledger holds under
//! the same or a later generation is a duplicate: answered `commandAck` and nothing else. One
//! whose operator the gate does not allow skips the ledger and the intake, is recorded as a
//! `preconditionFailed` event with the code `operatorNotAllowed` instead and is answered
//! `commandAck` the same way (ADR-483); one past its start deadline likewise skips them and is
//! recorded as an `expired` event (ADR-486). Every verified instruction raises the highest issue
//! time first, and the file that holds it is synced, when it is behind, before the record, the
//! intake, the append and the answer, a duplicate's included; a sync that fails withholds the
//! answer and closes with 1011. So does a ledger record that fails, an intake that fails and an
//! append that finds the outbox poisoned or fails to write (the session reopens the outbox and
//! the ledger before its next connection); an append refused as full keeps the connection and
//! leaves the command unanswered. A refusal is answered `commandRejected` with its reason and
//! records nothing. Answers are written on this connection through the one writer and under the
//! write bound.
//! A `logUploadCommand` is handed to [`crate::log_upload::LogUploads`] as a task beside the loop
//! (at most four at once, the rest in order, none repeated while it is in hand); the loop writes its `logUploadAnswer`
//! when the task is done, which is after an entry is durable, and a task that fails leaves the
//! command unanswered (ADR-508).
//! Sending an answer does not reset the idle deadline; only a frame received does. Never logged:
//! the token, a message's text, a `commandId`, a close reason the server sent.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use hyper::header::HeaderName;
use shared_proto::{
    AgentEventMessage, AgentMessage, CLOSE_CODE_BINARY, CLOSE_CODE_GOING_AWAY, CLOSE_CODE_IDLE,
    CLOSE_CODE_PROTOCOL, CLOSE_CODE_REPLACED, CLOSE_CODE_REVOKED, CLOSE_CODE_TOO_BIG,
    CONTROL_SUBPROTOCOL, FailureCode, HEARTBEAT_MISSED_LIMIT, JobId, JobState,
    MAX_CONTROL_MESSAGE_LEN, NONCE_HEADER, PROOF_HEADER, ServerMessage, close_reason,
};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, ClientRequestBuilder, Message, http};

use super::backoff::lived;
use super::dial::{self, ConnectError, Stream};
use super::events::{Acked, Sender};
use super::exchange::Token;
use super::gate::{Admitted, Gate, IntakeError};
use super::{Fail, Outcome, Phase, SessionState, Stop, Wiring};
use crate::answer_proof::Sent;
use crate::deadline::{MarkError, Verdict};
use crate::instruction::VerifiedInstruction;
use crate::log_upload::{LogUploadError, LogUploads};
use crate::outbox::{Kind, OutboxError};
use crate::received_jobs::LedgerError;
use crate::server_http;

/// The standard close code (RFC 6455 section 7.4.1) for an endpoint that cannot go on because of
/// a fault of its own: the agent's outbox cannot take events.
const CLOSE_CODE_INTERNAL: u16 = 1011;

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
    /// The 101 carried no valid proof (ADR-504): the loop backs off to the cap.
    pub(super) unproven: bool,
    /// The error that ended the attempt before a response, when there is one.
    pub(super) source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

fn fail(class: &'static str, status: Option<u16>) -> UpgradeFail {
    UpgradeFail {
        class,
        status,
        unproven: false,
        source: None,
    }
}

fn fail_with(
    class: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> UpgradeFail {
    UpgradeFail {
        source: Some(Box::new(source)),
        ..fail(class, None)
    }
}

/// Opens the WebSocket (D). The request has `Host` from the record, the bearer token, a fresh
/// `Ngr-Nonce` and the subprotocol, and no `Origin` and no extensions; the bounds are 5 seconds
/// to connect and 30 in all. The 101 is taken only with the server's answer proof (ADR-504): an
/// unproven one is dropped with its socket before a frame is written or read.
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
        .map_err(|e| fail_with("upgrade-request", e))?;
    let nonce = (w.random)().map_err(|e| fail_with("upgrade-random", e))?;
    let request = ClientRequestBuilder::new(uri)
        .with_header("Authorization", format!("Bearer {}", token.as_str()))
        .with_header(NONCE_HEADER, STANDARD.encode(nonce))
        .with_sub_protocol(CONTROL_SUBPROTOCOL);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_CONTROL_MESSAGE_LEN))
        .max_frame_size(Some(MAX_CONTROL_MESSAGE_LEN));
    let handshake = tokio_tungstenite::client_async_with_config(request, stream, Some(config));
    match tokio::time::timeout_at(deadline, handshake).await {
        Err(_) => Err(fail("upgrade-transport", None)),
        Ok(Ok((socket, response))) => {
            let proof = server_http::single_text(
                response.headers(),
                &HeaderName::from_static(PROOF_HEADER),
            );
            let sent = Sent {
                nonce,
                method: "GET",
                target: CONTROL_PATH,
                body: &[],
            };
            if w.verifier
                .proved(&sent, response.status().as_u16(), &[], proof.as_deref())
            {
                Ok(socket)
            } else {
                // Dropping the socket closes the connection without a frame.
                Err(UpgradeFail {
                    class: "upgrade-unproven",
                    status: Some(response.status().as_u16()),
                    unproven: true,
                    source: None,
                })
            }
        }
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
async fn write_text(socket: &mut Socket, text: &str) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(text.into()))).await,
        Ok(Ok(()))
    )
}

/// Writes one text message, unless a stop comes first. A stop does not wait for a stalled write:
/// the transport that stalls it would stall the Close as well, so the connection is dropped (H5).
/// The write is polled first, so one that completes at once is followed by Close 1001. `Ok` is
/// the message written; `Err` is the end the failure or the stop decides.
async fn write_or_stop(
    socket: &mut Socket,
    text: &str,
    stop: &Stop,
    lived_now: bool,
) -> Result<(), End> {
    let written = tokio::select! {
        biased;
        written = write_text(socket, text) => Some(written),
        () = stop.requested() => None,
    };
    match written {
        None => Err(End::stop_dropped()),
        Some(false) => Err(End::dropped("write", lived_now)),
        Some(true) => Ok(()),
    }
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

/// The reason whose word is the longest, so a `commandRejected` with it is the longest answer to a
/// job command (`commandAck` is shorter).
pub(super) const LONGEST_REASON: shared_proto::InstructionRejectReason =
    shared_proto::InstructionRejectReason::UnsupportedVersion;

/// Runs one connection to its end.
pub(super) async fn serve(mut socket: Socket, state: &mut SessionState<'_>) -> Outcome {
    let end = match await_hello(&mut socket, state.stop).await {
        Err(end) => end,
        Ok(secs) => {
            state.warnings.hello();
            tracing::info!(heartbeat_secs = secs, "control channel connected");
            state.status.connected(secs);
            let mut gate = Gate::new(
                &state.wiring.instruction_key,
                &state.wiring.record.agent_id,
                &state.wiring.record.owner_id,
                &state.wiring.record.preset,
            );
            let end = connected(&mut socket, secs, state, &mut gate).await;
            gate.finish();
            end
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
async fn connected(
    socket: &mut Socket,
    secs: u32,
    state: &mut SessionState<'_>,
    gate: &mut Gate<'_>,
) -> End {
    let w = state.wiring;
    let mut sender = Sender::new(&w.outbox);
    let mut asks = LogAsks::new(Arc::clone(&w.log_uploads));
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
                if let Err(end) = write_or_stop(socket, HEARTBEAT, state.stop, lived_now).await {
                    return end;
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
                    Ok(ServerMessage::JobCommand { command_id, signed_job }) => {
                        // The command is a received frame; its answer is not one. The
                        // connection's life is measured to here (the backoff reset rule).
                        last_frame_at = Instant::now();
                        let lived_now = lived(hello_at, last_frame_at);
                        if let Err(end) = job_command(
                            socket,
                            state,
                            gate,
                            command_id,
                            signed_job.as_deref(),
                            lived_now,
                        )
                        .await
                        {
                            return end;
                        }
                    }
                    Ok(ServerMessage::EventAck { seq }) => {
                        last_frame_at = Instant::now();
                        let acked = tokio::select! {
                            biased;
                            () = state.stop.requested() => return End::stop(),
                            acked = sender.ack(seq) => acked,
                        };
                        match acked {
                            Acked::Taken => {}
                            Acked::OutOfRange => {
                                let lived_now = lived(hello_at, last_frame_at);
                                return End::close(CLOSE_CODE_PROTOCOL, "event-ack", lived_now);
                            }
                            Acked::Poisoned => {
                                let lived_now = lived(hello_at, last_frame_at);
                                return End::close(
                                    CLOSE_CODE_INTERNAL,
                                    "outbox-poisoned",
                                    lived_now,
                                );
                            }
                        }
                    }
                    Ok(ServerMessage::LogUploadCommand { job_id, generation }) => {
                        last_frame_at = Instant::now();
                        asks.ask(job_id.0, generation);
                    }
                    Ok(
                        ServerMessage::CancelCommand { .. }
                        | ServerMessage::HmiResponse { .. }
                        | ServerMessage::SeedKeyResponse { .. },
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
            },
            Some(done) = asks.next(), if !asks.is_idle() => {
                let answer = match done {
                    Ok(Ok(answer)) => answer,
                    Ok(Err(error)) => {
                        state.warnings.log_upload_failure(&error);
                        continue;
                    }
                    Err(()) => {
                        state.warnings.log_upload_failure(&LogUploadError::Task(None));
                        continue;
                    }
                };
                // The answer is written only now, after the work has made its entry durable.
                let Ok(text) = answer.encode() else {
                    tracing::debug!("a log answer is over the size limit and is not sent");
                    continue;
                };
                if let Err(end) = write_or_stop(socket, &text, state.stop, lived_now).await {
                    return end;
                }
            }
            (seq, text) = sender.next() => {
                if let Err(end) = write_or_stop(socket, &text, state.stop, lived_now).await {
                    return end;
                }
                sender.wrote(seq);
            }
        }
    }
}

/// The class of a mark file that could not be written.
const MARK_WRITE: &str = "mark-write";
/// The class of a ledger record that could not be written.
const LEDGER_WRITE: &str = "ledger-write";
/// The class of a ledger that refuses records until it is opened again.
const LEDGER_POISONED: &str = "ledger-poisoned";
/// The class of an intake that could not take a recorded instruction.
const INTAKE: &str = "intake";

/// What a verified instruction comes to once its operator, its ledger record and its deadline are
/// judged.
enum Judged {
    /// New: recorded, handed to the intake, with a `received` event.
    Accepted(Box<VerifiedInstruction>),
    /// The operator is not allowed (ADR-483).
    Refused { job_id: String },
    /// Past the start deadline (ADR-486).
    Expired { job_id: String },
    /// The ledger holds the job ID under the same or a later generation (ADR-489): acknowledged,
    /// nothing else.
    Duplicate,
}

/// The operator check's verdict, then the ledger's, then the deadline's. The device clock is read
/// only for an instruction whose operator is allowed and which the ledger does not hold, and once.
fn judge(w: &Wiring, gate: &mut Gate<'_>, admitted: Admitted) -> Judged {
    match admitted {
        Admitted::OperatorNotAllowed { job_id, .. } => Judged::Refused { job_id },
        Admitted::Accepted(verified) => {
            let instruction = verified.instruction();
            let generation = instruction.ownership_generation;
            if let Some(recorded) = w.ledger.highest(&instruction.job_id.0) {
                if generation == recorded {
                    gate.duplicate(&instruction.job_id.0, generation);
                    return Judged::Duplicate;
                }
                if generation < recorded {
                    gate.earlier_generation(&instruction.job_id.0, generation);
                    return Judged::Duplicate;
                }
            }
            let deadline = instruction.start_deadline;
            match w.marks.judge(&*w.clock, deadline) {
                Verdict::Open => Judged::Accepted(verified),
                Verdict::Expired { by_mark } => {
                    let job_id = verified.instruction().job_id.0.clone();
                    gate.expired(&job_id, by_mark);
                    Judged::Expired { job_id }
                }
            }
        }
    }
}

/// One job command, from its verification to its answer (design 5.3, ADR-461 Decision item 9,
/// ADR-489). The order is: verify, raise the mark, the operator check, the duplicate check, the
/// deadline check, sync the mark, record the job, hand it to the intake, append the event, write
/// the answer. `Err` is the end of the connection; `Ok` is a command that was answered, or that
/// was left unanswered with the connection standing (its redelivery is the retry).
async fn job_command(
    socket: &mut Socket,
    state: &mut SessionState<'_>,
    gate: &mut Gate<'_>,
    command_id: String,
    signed_job: Option<&str>,
    lived_now: bool,
) -> Result<(), End> {
    let w = state.wiring;
    // A `commandId` whose longest possible answer the codec refuses (one near the size limit) is
    // not usable, as a missing one is not: the gate and the intake see nothing and the
    // connection closes with 1002.
    let longest = AgentMessage::CommandRejected {
        command_id: command_id.clone(),
        reason: LONGEST_REASON,
    };
    if longest.encode().is_err() {
        return Err(End::close(CLOSE_CODE_PROTOCOL, "command-id", lived_now));
    }
    let (rejected, admitted) = match gate.admit(signed_job) {
        Ok(admitted) => (None, Some(admitted)),
        Err(reason) => (Some(reason), None),
    };
    let answer = match rejected {
        None => AgentMessage::CommandAck { command_id },
        Some(reason) => AgentMessage::CommandRejected { command_id, reason },
    };
    let Ok(text) = answer.encode() else {
        // Not reachable once the longest answer encodes.
        return Err(End::close(CLOSE_CODE_PROTOCOL, "command-id", lived_now));
    };
    // The highest issue time rises with every instruction that verified, whatever becomes of it
    // (ADR-486).
    let judged = admitted.map(|admitted| {
        w.marks.raise(admitted.issued_at());
        judge(w, gate, admitted)
    });
    // The mark is synced before anything the command leads to is recorded, handed over or
    // answered, a duplicate included.
    if judged.is_some() {
        match sync_marks(w, state.stop).await {
            Synced::Done => {}
            Synced::Stopped => return Err(End::stop()),
            Synced::Failed(error) => {
                state.warnings.mark_failure(&error);
                return Err(End::close(CLOSE_CODE_INTERNAL, MARK_WRITE, lived_now));
            }
        }
    }
    // A new job is recorded in the ledger first, then the intake has it, and the outbox has the
    // `received` event, all before the ack is written (ADR-489, ADR-478 item 5). An instruction
    // whose operator is not allowed skips both and gets one terminal event instead of
    // `received`, under the same bound and with the same failure handling (ADR-483); so does one
    // past its deadline (ADR-486). Neither is recorded. A duplicate gets none of this.
    let handled = match judged {
        None | Some(Judged::Duplicate) => Handled::Done,
        Some(Judged::Accepted(verified)) => take_job(w, state.stop, verified).await,
        Some(Judged::Refused { job_id }) => {
            let refused = JobState::PreconditionFailed {
                code: FailureCode::OperatorNotAllowed,
                detail: String::new(),
            };
            Handled::from(record_job_state(w, state.stop, job_id, refused).await)
        }
        Some(Judged::Expired { job_id }) => {
            Handled::from(record_job_state(w, state.stop, job_id, JobState::Expired).await)
        }
    };
    match handled {
        Handled::Done => {}
        Handled::Stopped => return Err(End::stop()),
        Handled::Ledger(class, error) => {
            state.warnings.ledger_failure(class, &error);
            // A ledger that failed to write refuses records until it is opened again, which the
            // session does before its next connection: end this one.
            return Err(End::close(CLOSE_CODE_INTERNAL, class, lived_now));
        }
        Handled::Intake(error) => {
            state.warnings.intake_failure(&error);
            return Err(End::close(CLOSE_CODE_INTERNAL, INTAKE, lived_now));
        }
        Handled::Event(class, error) => {
            state.warnings.event_failure(class, &error);
            // A poisoned outbox cannot take any event until it is reopened, which the session
            // does before its next connection (ADR-478 item 4): end this one.
            if matches!(class, "outbox-poisoned" | "outbox-write") {
                return Err(End::close(CLOSE_CODE_INTERNAL, class, lived_now));
            }
            return Ok(());
        }
    }
    write_or_stop(socket, &text, state.stop, lived_now).await?;
    match rejected {
        None => state.status.command_acknowledged(),
        Some(_) => state.status.command_rejected(),
    }
    Ok(())
}

/// What the work between the mark and the answer came to.
enum Handled {
    Done,
    /// The stop came first: a write that started goes on unobserved.
    Stopped,
    /// The ledger record failed: the class and the error.
    Ledger(&'static str, LedgerError),
    /// The intake failed after the record was made.
    Intake(IntakeError),
    /// The event could not be appended: the class and the error.
    Event(&'static str, OutboxError),
}

impl From<Recorded> for Handled {
    fn from(recorded: Recorded) -> Handled {
        match recorded {
            Recorded::Done => Handled::Done,
            Recorded::Stopped => Handled::Stopped,
            Recorded::Failed(class, error) => Handled::Event(class, error),
        }
    }
}

/// A new job: its record in the ledger, made durable on a blocking task raced with the stop; then
/// the intake, raced with the stop; then the `received` event. A step that fails stops the
/// sequence, and a record that was made stays (the redelivery is a duplicate, ADR-489).
async fn take_job(w: &Wiring, stop: &Stop, verified: Box<VerifiedInstruction>) -> Handled {
    let job_id = verified.instruction().job_id.0.clone();
    let generation = verified.instruction().ownership_generation;
    let ledger = w.ledger.clone();
    let (record_id, envelope) = (job_id.clone(), verified.envelope().to_vec());
    let write =
        tokio::task::spawn_blocking(move || ledger.record(&record_id, generation, &envelope));
    let done = tokio::select! {
        biased;
        () = stop.requested() => return Handled::Stopped,
        done = write => done,
    };
    match done {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let class = match error {
                LedgerError::Poisoned => LEDGER_POISONED,
                _ => LEDGER_WRITE,
            };
            return Handled::Ledger(class, error);
        }
        Err(_) => {
            let error = LedgerError::Io(std::io::ErrorKind::Other);
            return Handled::Ledger(LEDGER_WRITE, error);
        }
    }
    let taken = tokio::select! {
        biased;
        () = stop.requested() => return Handled::Stopped,
        taken = w.intake.take(*verified) => taken,
    };
    if let Err(error) = taken {
        return Handled::Intake(error);
    }
    Handled::from(record_job_state(w, stop, job_id, JobState::Received).await)
}

/// What making the mark durable came to.
enum Synced {
    Done,
    /// The stop came first: a write that started goes on unobserved.
    Stopped,
    Failed(MarkError),
}

/// Writes the mark file when it is behind, on a blocking task, raced with the stop. The write
/// holds the store (and its lock) until it ends, so a session started after a stop finds it in
/// use and not half written.
async fn sync_marks(w: &Wiring, stop: &Stop) -> Synced {
    if !w.marks.owes_write() {
        return Synced::Done;
    }
    let marks = w.marks.clone();
    let write = tokio::task::spawn_blocking(move || marks.sync());
    let done = tokio::select! {
        biased;
        () = stop.requested() => return Synced::Stopped,
        done = write => done,
    };
    match done {
        Ok(Ok(())) => Synced::Done,
        Ok(Err(error)) => Synced::Failed(error),
        Err(_) => Synced::Failed(MarkError::Io(std::io::ErrorKind::Other)),
    }
}

/// What recording a job's first event came to.
enum Recorded {
    Done,
    /// The stop came first: the append, if it started, goes on unobserved.
    Stopped,
    /// The warning class and the error; the command is not acknowledged.
    Failed(&'static str, OutboxError),
}

/// The text of the first event of `job_id` with number `seq`: the job's `state`, `received`, the
/// terminal `preconditionFailed` of a refusal (ADR-483) or the terminal `expired` (ADR-486). No device time is given: the property
/// is optional and the server stamps its own receive time (16.3).
fn first_event(
    job_id: &str,
    seq: u64,
    state: &shared_proto::JobState,
) -> Result<String, shared_proto::ControlError> {
    AgentMessage::AgentEvent(AgentEventMessage {
        seq: Some(seq),
        occurred_at_agent: None,
        event: state.to_event_kind(JobId(job_id.to_owned())),
    })
    .encode()
}

/// Appends the job's first event on a blocking task, raced with the stop (ADR-478 item 5):
/// `received`, or the refusal's or the expiry's terminal event. All are `Kind::Received` for the admission bound.
/// It is tried once per command: the server's redelivery after the lease is the retry.
async fn record_job_state(w: &Wiring, stop: &Stop, job_id: String, state: JobState) -> Recorded {
    // The longest number the counter gives, so the text that is appended encodes as well.
    if let Err(error) = first_event(&job_id, u64::MAX - 1, &state) {
        let error = OutboxError::Io(std::io::Error::other(error.to_string()));
        return Recorded::Failed("outbox-event", error);
    }
    let append = w.outbox.append(Kind::Received, move |seq| {
        first_event(&job_id, seq, &state).unwrap_or_default()
    });
    let appended = tokio::select! {
        biased;
        () = stop.requested() => return Recorded::Stopped,
        appended = append => appended,
    };
    match appended {
        Ok(_) => Recorded::Done,
        Err(error) => {
            let class = match error {
                OutboxError::Full => "outbox-full",
                OutboxError::Poisoned => "outbox-poisoned",
                OutboxError::TooLarge | OutboxError::FrameTooLarge(..) => "outbox-event",
                _ => "outbox-write",
            };
            Recorded::Failed(class, error)
        }
    }
}

/// The log requests being worked on for one connection. Each runs as a task of its own beside the
/// loop, which writes its answer when it is done, so reading and heartbeats go on while a large
/// log is hashed. A request past the running limit waits in a queue, in the order asked. When the
/// connection ends the tasks are aborted and the flag handed to each is set: a read that was
/// blocking stops at its next chunk, releases the log's writer lock and enqueues nothing, and the
/// server's next command starts it again.
struct LogAsks {
    uploads: Arc<dyn LogUploads>,
    tasks: tokio::task::JoinSet<(
        String,
        u64,
        Result<shared_proto::LogUploadOutcome, LogUploadError>,
    )>,
    /// The request of each running task, so that a repeat of one in flight is dropped and not
    /// answered `busy` by the writer lock the first one holds.
    keys: HashMap<tokio::task::Id, (String, u64)>,
    /// Requests waiting for a running one to end.
    waiting: VecDeque<(String, u64)>,
    /// Set when this ends: the connection is over.
    cancel: Arc<AtomicBool>,
}

/// The most log requests worked on at once.
const MAX_LOG_ASKS: usize = 4;

/// The most requests that wait: the most one server request names.
const MAX_WAITING_LOG_ASKS: usize = 32;

impl Drop for LogAsks {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl LogAsks {
    fn new(uploads: Arc<dyn LogUploads>) -> Self {
        Self {
            uploads,
            tasks: tokio::task::JoinSet::new(),
            keys: HashMap::new(),
            waiting: VecDeque::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_idle(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Starts the request, queues it behind the running ones, or drops it when it is already
    /// running or waiting, or the queue is full (the server asks again).
    fn ask(&mut self, job_id: String, generation: u64) {
        let known = |(job, gen_): &(String, u64)| *job == job_id && *gen_ == generation;
        if self.keys.values().any(known) || self.waiting.iter().any(known) {
            tracing::debug!("a log request was dropped: it is already in hand");
        } else if self.keys.len() < MAX_LOG_ASKS {
            self.start(job_id, generation);
        } else if self.waiting.len() < MAX_WAITING_LOG_ASKS {
            self.waiting.push_back((job_id, generation));
        } else {
            tracing::debug!("a log request was dropped: too many are waiting");
        }
    }

    fn start(&mut self, job_id: String, generation: u64) {
        let uploads = Arc::clone(&self.uploads);
        let cancel = Arc::clone(&self.cancel);
        let key = (job_id.clone(), generation);
        let id = self
            .tasks
            .spawn(async move {
                let result = uploads.handle(job_id.clone(), generation, cancel).await;
                (job_id, generation, result)
            })
            .id();
        self.keys.insert(id, key);
    }

    /// The next finished request's answer, `Ok(Err(_))` for one that failed and `Err(())` for a
    /// task that panicked or was cancelled. A waiting request takes the place of the finished one.
    async fn next(&mut self) -> Option<Result<Result<AgentMessage, LogUploadError>, ()>> {
        let done = self.tasks.join_next_with_id().await?;
        let finished = match done {
            Ok((id, (job_id, generation, result))) => {
                self.keys.remove(&id);
                Ok(result.map(|outcome| AgentMessage::LogUploadAnswer {
                    job_id: JobId(job_id),
                    generation,
                    outcome,
                }))
            }
            Err(error) => {
                self.keys.remove(&error.id());
                Err(())
            }
        };
        while self.keys.len() < MAX_LOG_ASKS {
            let Some((job_id, generation)) = self.waiting.pop_front() else {
                break;
            };
            self.start(job_id, generation);
        }
        Some(finished)
    }
}
