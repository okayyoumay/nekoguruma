//! The flow's cases that need the key, a directory and a server, in process (the binary's are in
//! `crates/agent/tests/register.rs`).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use shared_crypto::registration::{
    Preset, RegistrationTranscript, ResponseTranscript, sign_response, verify_registration,
};
use shared_crypto::{PublicKey, Signature, Signer, SigningKey};

use super::record::{RECORD_FILE, TEMP_FILE};
use super::*;
use crate::identity::load_or_create;
use crate::identity::store::{MemoryStore, SecretName};

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";

/// An agent key that was stored by an earlier call, as a registering agent's is.
fn existing_key() -> AgentKey {
    let store = MemoryStore::new();
    let name = SecretName::agent_key(OperatingMode::User);
    let lock_dir = tempfile::tempdir().unwrap();
    let no_dir = || Ok(lock_dir.path().to_owned());
    let created = load_or_create(&store, &name, &no_dir, Duration::from_millis(100)).unwrap();
    assert!(created.created());
    let key = load_or_create(&store, &name, &no_dir, Duration::from_millis(100)).unwrap();
    assert!(!key.created());
    key
}

/// A server that answers its first connection as an honest one would, then returns what it read.
struct Stub {
    port: u16,
    server: SigningKey,
    seen: std::thread::JoinHandle<String>,
}

impl Stub {
    /// `status` is answered; a 201 carries a valid signed answer.
    fn start(status: u16) -> Stub {
        Stub::start_hooked(status, || {})
    }

    /// As [`Stub::start`], running `hook` after the request was read and before the answer.
    fn start_hooked(status: u16, hook: impl FnOnce() + Send + 'static) -> Stub {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server =
            SigningKey::from_pkcs8_der(&SigningKey::generate().unwrap().to_pkcs8_der()).unwrap();
        let signer = SigningKey::from_pkcs8_der(&server.to_pkcs8_der()).unwrap();
        let seen = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut byte = [0u8; 1];
            while !raw.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                raw.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&raw).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    l.strip_prefix("Content-Length: ")
                        .or_else(|| l.strip_prefix("content-length: "))
                })
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).unwrap();
            hook();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let reply = if status == 201 {
                let code = RegistrationCode::parse(request["code"].as_str().unwrap()).unwrap();
                let agent =
                    PublicKey::from_spki_pem(request["publicKey"].as_str().unwrap()).unwrap();
                let transcript = ResponseTranscript::new(
                    &code,
                    &agent,
                    AGENT_ID,
                    "owner-1",
                    Preset::Personal,
                    false,
                )
                .unwrap();
                let signature = sign_response(&signer, &transcript).unwrap();
                let key = signer.public_key();
                let json = serde_json::json!({
                    "agentId": AGENT_ID,
                    "ownerId": "owner-1",
                    "preset": "personal",
                    "allowRemote": false,
                    "instructionKey": key.to_spki_pem(),
                    "instructionKeyId": key.fingerprint_hex(),
                    "signature": STANDARD.encode(signature.to_bytes()),
                })
                .to_string();
                format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                )
            } else {
                format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            };
            stream.write_all(reply.as_bytes()).unwrap();
            format!("{head}{}", String::from_utf8_lossy(&body))
        });
        Stub { port, server, seen }
    }

    fn request(&self, replace: bool) -> Request {
        Request {
            mode: OperatingMode::User,
            server: ServerOrigin::parse(&format!("http://127.0.0.1:{}", self.port)).unwrap(),
            server_key: self.server.public_key().fingerprint(),
            replace,
        }
    }
}

fn run(
    request: &Request,
    key: &AgentKey,
    code: &RegistrationCode,
    dir: &Path,
) -> Result<Registered, RegisterError> {
    register_in(request, key, code, dir, Timeouts::default())
}

fn code() -> RegistrationCode {
    RegistrationCode::generate().unwrap()
}

/// The request the stub saw is the documented one: the headers, and a proof that verifies.
#[test]
fn a_registration_is_sent_once_and_stored() {
    let dir = tempfile::tempdir().unwrap();
    let key = existing_key();
    let code = code();
    let stub = Stub::start(201);
    let request = stub.request(false);
    let done = run(&request, &key, &code, dir.path()).unwrap();
    assert!(!done.replaced);
    assert_eq!(done.durability, Durability::Synced);

    let seen = stub.seen.join().unwrap();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(lines[0], "POST /api/v1/agents/register HTTP/1.1");
    let lower = seen.to_ascii_lowercase();
    assert!(lower.contains(&format!("host: 127.0.0.1:{}\r\n", request.server.port())));
    assert!(lower.contains("connection: close\r\n"));
    assert!(!lower.contains("origin:"));
    let body: serde_json::Value =
        serde_json::from_str(seen.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["mode"], "user");
    assert_eq!(body["code"], code.as_str());
    let public = PublicKey::from_spki_pem(body["publicKey"].as_str().unwrap()).unwrap();
    assert_eq!(public, key.public_key());
    let proof =
        Signature::from_bytes(&STANDARD.decode(body["proof"].as_str().unwrap()).unwrap()).unwrap();
    let transcript = RegistrationTranscript::new(&code, &public, Mode::User, request.server_key);
    verify_registration(&public, &transcript, &proof).unwrap();

    let stored = Record::read(&dir.path().join(RECORD_FILE)).unwrap();
    assert_eq!(stored, done.record);
    assert_eq!(stored.agent_id, AGENT_ID);
    assert_eq!(stored.server_origin, request.server.as_str());
    assert_eq!(
        stored.agent_key_fingerprint,
        key.public_key().fingerprint_hex()
    );
    assert!(!dir.path().join(TEMP_FILE).exists());
    let output = done.output().to_string();
    assert!(!output.contains(code.as_str()));
    assert!(!output.contains("BEGIN"));
}

/// L2: a second registration finds the lock held and sends nothing.
#[test]
fn a_held_lock_is_exit_3_and_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let _held = WriterLock::at(&dir.path().join(LOCK_FILE)).unwrap();
    let stub = Stub::start(201);
    let error = run(&stub.request(false), &existing_key(), &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert_eq!(error.to_string(), "another registration is running");
    assert!(
        std::fs::read_dir(dir.path()).unwrap().count() == 1,
        "only the lock file"
    );
    // The stub never got a connection: unblock its accept.
    let _ = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, stub.port));
    let _ = stub.seen.join();
}

/// L3: the lock is released when its holder goes, and the file stays.
#[test]
fn the_lock_is_released_after_the_command_and_its_file_stays() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start(403);
    let _ = run(&stub.request(false), &existing_key(), &code(), dir.path());
    assert!(dir.path().join(LOCK_FILE).is_file());
    assert!(WriterLock::at(&dir.path().join(LOCK_FILE)).is_ok());
    let _ = stub.seen.join();
}

/// R1, R2, R3.
#[test]
fn an_existing_record_stops_the_command_unless_replace_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(RECORD_FILE);
    std::fs::write(&path, b"anything at all").unwrap();
    let stub = Stub::start(201);
    let key = existing_key();
    let error = run(&stub.request(false), &key, &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 5);
    assert_eq!(std::fs::read(&path).unwrap(), b"anything at all");
    assert!(!dir.path().join(TEMP_FILE).exists());

    let done = run(&stub.request(true), &key, &code(), dir.path()).unwrap();
    assert!(done.replaced);
    assert_eq!(Record::read(&path).unwrap(), done.record);
    let _ = stub.seen.join();
}

/// R4.
#[test]
fn a_record_place_that_is_not_a_regular_file_is_exit_3_with_or_without_replace() {
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(RECORD_FILE)).unwrap();
        let stub = Stub::start(201);
        let error = run(&stub.request(replace), &existing_key(), &code(), dir.path()).unwrap_err();
        assert_eq!(error.exit_code(), 3);
        let _ = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, stub.port));
        let _ = stub.seen.join();
    }
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(RECORD_FILE)).unwrap();
        let stub = Stub::start(201);
        let error = run(&stub.request(true), &existing_key(), &code(), dir.path()).unwrap_err();
        assert_eq!(error.exit_code(), 3);
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
        let _ = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, stub.port));
        let _ = stub.seen.join();
    }
}

/// R5: a temp that cannot be made is exit 3 before anything is sent.
#[test]
fn a_temp_that_cannot_be_made_is_exit_3_before_anything_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(TEMP_FILE)).unwrap();
    std::fs::write(dir.path().join(TEMP_FILE).join("x"), b"x").unwrap();
    let stub = Stub::start(201);
    let error = run(&stub.request(false), &existing_key(), &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(error.to_string().contains("nothing was sent"));
    let _ = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, stub.port));
    let _ = stub.seen.join();
}

/// T: each status, and that the record is left as it was and no temp stays.
#[test]
fn each_status_has_its_exit_and_leaves_the_record_alone() {
    let key = existing_key();
    let cases: [(u16, u8, &str); 9] = [
        (403, 1, "refused: the code is wrong"),
        (429, 1, "refused: the code is wrong"),
        (400, 1, "unexpected answer (status 400)"),
        (421, 1, "--server is not the server's own origin"),
        (307, 1, "redirect, not followed"),
        (408, 3, "outcome unknown"),
        (500, 3, "outcome unknown"),
        (503, 3, "outcome unknown"),
        (200, 1, "unexpected answer (status 200)"),
    ];
    for (status, exit, text) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RECORD_FILE), b"old").unwrap();
        let stub = Stub::start(status);
        let error = run(&stub.request(true), &key, &code(), dir.path()).unwrap_err();
        assert_eq!(error.exit_code(), exit, "{status}");
        assert!(error.to_string().contains(text), "{status}: {error}");
        assert_eq!(std::fs::read(dir.path().join(RECORD_FILE)).unwrap(), b"old");
        assert!(!dir.path().join(TEMP_FILE).exists());
        let _ = stub.seen.join();
    }
}

/// T3: `Retry-After` is shown only as an integer from 1 to 60.
#[test]
fn the_retry_after_is_shown_only_when_it_is_a_small_integer() {
    let key = existing_key();
    let agent = key.public_key();
    let code = code();
    let reply = |retry: Option<&str>| Reply {
        status: 429,
        retry_after: retry.map(str::to_owned),
        json_content_type: false,
        body: None,
    };
    for (retry, wait) in [
        (Some("1"), Some("wait 1 s")),
        (Some("60"), Some("wait 60 s")),
        (Some("0"), None),
        (Some("61"), None),
        (Some("-5"), None),
        (Some("Wed, 21 Oct 2015 07:28:00 GMT"), None),
        (Some("99999999999999999999"), None),
        (Some(""), None),
        (None, None),
    ] {
        let text = judge(&reply(retry), &code, &agent, [0; 32])
            .unwrap_err()
            .to_string();
        match wait {
            Some(wait) => assert!(text.ends_with(wait), "{retry:?}: {text}"),
            None => assert!(!text.contains("wait"), "{retry:?}: {text}"),
        }
        assert!(!text.contains("Wed"), "{text}");
    }
}

/// K4 and V7 through the flow: nothing is stored.
#[test]
fn an_answer_from_another_key_is_exit_6_and_nothing_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start(201);
    let mut request = stub.request(false);
    // The operator was given another server's key ID.
    request.server_key = SigningKey::generate().unwrap().public_key().fingerprint();
    let error = run(&request, &existing_key(), &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 6);
    assert!(!dir.path().join(RECORD_FILE).exists());
    assert!(!dir.path().join(TEMP_FILE).exists());
    let _ = stub.seen.join();
}

/// H1.
#[test]
fn a_refused_connection_is_exit_3_with_nothing_sent() {
    let dir = tempfile::tempdir().unwrap();
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let request = Request {
        mode: OperatingMode::User,
        server: ServerOrigin::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
        server_key: [1; 32],
        replace: false,
    };
    let error = run(&request, &existing_key(), &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(error.to_string().ends_with("nothing was sent"));
    assert!(!dir.path().join(TEMP_FILE).exists());
}

/// W3 as the flow reports it: the server's answer was good and the store failed. The old
/// record stays and no temp does.
#[test]
fn a_store_that_fails_after_the_answer_says_a_new_code_is_needed() {
    let dir = tempfile::tempdir().unwrap();
    let place = dir.path().join(RECORD_FILE);
    // A directory takes the record's place while the request is in flight: the rename fails.
    let stub = Stub::start_hooked(201, {
        let place = place.clone();
        move || std::fs::create_dir(place).unwrap()
    });
    let error = run(&stub.request(false), &existing_key(), &code(), dir.path()).unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(
        error
            .to_string()
            .starts_with("registered at the server but not stored")
    );
    assert!(error.to_string().ends_with("a new code is needed"));
    assert!(place.is_dir());
    assert!(!dir.path().join(TEMP_FILE).exists());
    let _ = stub.seen.join();
}
