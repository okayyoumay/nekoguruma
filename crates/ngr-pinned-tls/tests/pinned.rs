//! The handshake against a TLS server on an in-memory pipe, for each certificate a server can
//! present.

use std::io;

use ngr_pinned_tls::test_server::{Acceptor, Identity, Key};
use ngr_pinned_tls::{CertPin, Refused, connect};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

fn pin_of(identity: &Identity) -> CertPin {
    CertPin::of_certificate(identity.certificate())
}

/// Connects a client with `pin` to `acceptor` over a new pipe. Returns the client's result, and
/// what the server side saw: the bytes of application data it read, or the handshake error.
async fn connect_to(
    acceptor: &Acceptor,
    pin: &CertPin,
) -> (Result<Vec<u8>, Refused>, io::Result<Vec<u8>>) {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let server = async {
        let mut stream = acceptor.accept(server).await?;
        let mut seen = Vec::new();
        stream.read_to_end(&mut seen).await?;
        stream.write_all(b"pong").await?;
        stream.shutdown().await?;
        Ok(seen)
    };
    let client = async {
        let mut stream = connect(pin, client).await?;
        // The pipe's ends, not the TLS stream's, carry the bytes; a failed write is a refusal
        // of the test's own making.
        stream.write_all(b"ping").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).await.unwrap();
        Ok(answer)
    };
    tokio::join!(client, server)
}

/// The handshake is refused and the server received nothing but a failed handshake.
async fn assert_refused(acceptor: &Acceptor, pin: &CertPin) -> Refused {
    let (client, server) = connect_to(acceptor, pin).await;
    assert!(
        server.is_err(),
        "the server saw application data: {server:?}"
    );
    client.expect_err("the handshake was accepted")
}

#[tokio::test]
async fn the_right_certificate_and_key_connect_and_carry_data() {
    for key in [Key::P256, Key::P384] {
        let identity = Identity::generate(key);
        let (client, server) = connect_to(&Acceptor::new(&identity), &pin_of(&identity)).await;
        assert_eq!(client.unwrap(), b"pong", "{key:?}");
        assert_eq!(server.unwrap(), b"ping", "{key:?}");
    }
}

#[tokio::test]
async fn another_certificate_is_refused_and_receives_no_application_byte() {
    let served = Identity::generate(Key::P256);
    let other = Identity::generate(Key::P256);
    let refused = assert_refused(&Acceptor::new(&served), &pin_of(&other)).await;
    assert_eq!(refused, Refused::Certificate);
}

#[tokio::test]
async fn the_right_certificate_with_another_p256_key_fails_on_the_signature() {
    let real = Identity::generate(Key::P256);
    let other = Identity::generate(Key::P256);
    let impostor = Acceptor::presenting(&[real.certificate()], &other);
    // The pin is right, so only the signature can be what refuses.
    let refused = assert_refused(&impostor, &pin_of(&real)).await;
    assert_eq!(refused, Refused::Signature);
}

#[tokio::test]
async fn the_right_certificate_with_a_p384_key_is_refused() {
    let real = Identity::generate(Key::P256);
    let other = Identity::generate(Key::P384);
    let impostor = Acceptor::presenting(&[real.certificate()], &other);
    let refused = assert_refused(&impostor, &pin_of(&real)).await;
    assert_ne!(refused, Refused::Certificate, "the pin was right");
}

#[tokio::test]
async fn certificates_after_the_leaf_carry_no_authority() {
    let real = Identity::generate(Key::P256);
    let extra = Identity::generate(Key::P256);
    let pin = pin_of(&real);

    let behind = Acceptor::presenting(&[real.certificate(), extra.certificate()], &real);
    let (client, server) = connect_to(&behind, &pin).await;
    assert_eq!(
        (client.unwrap(), server.unwrap()),
        (b"pong".to_vec(), b"ping".to_vec())
    );

    // The pinned certificate is not the leaf, whatever else it presents.
    let wrong_first = Acceptor::presenting(&[extra.certificate(), real.certificate()], &extra);
    assert_eq!(
        assert_refused(&wrong_first, &pin).await,
        Refused::Certificate
    );
}

#[tokio::test]
async fn a_second_connection_is_judged_against_its_own_pin() {
    let identity = Identity::generate(Key::P256);
    let acceptor = Acceptor::new(&identity);
    let (client, _) = connect_to(&acceptor, &pin_of(&identity)).await;
    client.unwrap();
    let other = pin_of(&Identity::generate(Key::P256));
    assert_eq!(
        assert_refused(&acceptor, &other).await,
        Refused::Certificate
    );
}

#[tokio::test]
async fn validity_dates_and_names_are_not_checked() {
    for (what, identity) in [
        ("expired", Identity::expired()),
        ("not yet valid", Identity::not_yet_valid()),
        ("no name", Identity::without_name()),
    ] {
        let (client, server) = connect_to(&Acceptor::new(&identity), &pin_of(&identity)).await;
        assert_eq!(client.unwrap(), b"pong", "{what}");
        assert_eq!(server.unwrap(), b"ping", "{what}");
    }
}

#[tokio::test]
async fn a_plain_http_server_is_refused_before_any_request() {
    let pin = pin_of(&Identity::generate(Key::P256));
    let (client, server) = tokio::io::duplex(16 * 1024);
    let plain = tokio::spawn(async move {
        let mut server: DuplexStream = server;
        let mut received = vec![0u8; 4096];
        let n = server.read(&mut received).await.unwrap();
        received.truncate(n);
        server
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let mut rest = Vec::new();
        server.read_to_end(&mut rest).await.unwrap();
        (received, rest)
    });
    let refused = connect(&pin, client)
        .await
        .expect_err("accepted a plain server");
    assert_eq!(refused, Refused::Handshake(io::ErrorKind::InvalidData));
    let (hello, rest) = plain.await.unwrap();
    assert_eq!(
        hello.first(),
        Some(&0x16),
        "what it got was a TLS handshake record"
    );
    // Only a TLS alert (record type 21) may follow; application data is type 23.
    assert!(
        rest.is_empty() || rest[0] == 0x15,
        "something but an alert followed the hello: {rest:?}"
    );
}

#[tokio::test]
async fn a_close_during_the_handshake_is_a_refusal() {
    let pin = pin_of(&Identity::generate(Key::P256));
    let (client, mut server) = tokio::io::duplex(16 * 1024);
    let closer = tokio::spawn(async move {
        let mut hello = vec![0u8; 4096];
        let n = server.read(&mut hello).await.unwrap();
        assert!(n > 0, "the client sent a hello");
        // Dropping `server` closes the pipe.
    });
    let refused = connect(&pin, client)
        .await
        .expect_err("accepted a closed stream");
    closer.await.unwrap();
    assert_eq!(refused, Refused::Handshake(io::ErrorKind::UnexpectedEof));
}
