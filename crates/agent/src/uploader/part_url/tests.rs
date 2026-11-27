use super::*;

fn origin() -> ServerOrigin {
    ServerOrigin::parse("http://127.0.0.1:8080").unwrap()
}

fn pin() -> CertPin {
    CertPin::of_certificate(b"a certificate")
}

fn accepted(url: &str) -> PartTarget {
    check(&origin(), url, pin()).unwrap_or_else(|error| panic!("{url:?}: {error}"))
}

#[test]
fn a_url_on_the_registered_host_is_accepted_with_its_port_and_target() {
    let t = accepted("https://127.0.0.1:9000/bucket/key?X-Sig=abc%2Fd&n=1");
    assert_eq!(t.port, 9000);
    assert_eq!(t.host, "127.0.0.1:9000");
    assert_eq!(t.target, "/bucket/key?X-Sig=abc%2Fd&n=1");
    assert_eq!(t.pin(), &pin());
    // No port is port 443, whatever the origin's port is.
    let t = accepted("https://127.0.0.1/a");
    assert_eq!(
        (t.port, t.host.as_str(), t.target.as_str()),
        (443, "127.0.0.1", "/a")
    );
    // The ends of the port range, and the origin's own port.
    assert_eq!(accepted("https://127.0.0.1:1/x").port, 1);
    assert_eq!(accepted("https://127.0.0.1:65535/x").port, 65535);
    assert_eq!(accepted("https://127.0.0.1:8080/x").port, 8080);
    // The target is the text from the first slash, unchanged, whatever it holds.
    assert_eq!(
        accepted("https://127.0.0.1:9000//a@b:80/c").target,
        "//a@b:80/c"
    );
}

/// The refusal of a port that is canonical digits but does not fit 16 bits.
fn out_of_range(digits: &str) -> Refused {
    Refused::PortOutOfRange(digits.parse::<u16>().unwrap_err())
}

#[test]
fn a_port_beyond_16_bits_keeps_the_parsers_error_as_its_source() {
    let error = check(&origin(), "https://127.0.0.1:65536/x", pin()).unwrap_err();
    assert!(std::error::Error::source(&error).is_some(), "{error:?}");
    assert_eq!(
        error.to_string(),
        Refused::Port.to_string(),
        "the text is the same as for any other bad port"
    );
}

#[test]
fn the_host_rule_refuses_what_is_not_the_registered_host() {
    let table: &[(&str, Refused)] = &[
        ("https://127.0.0.10/x", Refused::Origin),
        ("https://127.0.0.10:9000/x", Refused::Origin),
        ("https://127.0.0.1.example/x", Refused::Origin),
        ("https://127.0.0.1.:9000/x", Refused::Origin),
        ("https://127.0.0.1:80@x/y", Refused::Port),
        ("https://127.0.0.1@evil.example/x", Refused::Origin),
        ("https://user:pw@127.0.0.1:9000/x", Refused::Origin),
        ("https://127.0.0.1:9000@evil.example/x", Refused::Port),
        ("http://127.0.0.1:9000/x", Refused::Origin),
        ("http://127.0.0.1/x", Refused::Origin),
        ("HTTPS://127.0.0.1:9000/x", Refused::Origin),
        ("Https://127.0.0.1:9000/x", Refused::Origin),
        ("HTTP://127.0.0.1:9000/x", Refused::Origin),
        ("https://LOCALHOST:9000/x", Refused::Origin),
        ("https://localhost:9000/x", Refused::Origin),
        ("https://[::1]:9000/x", Refused::Origin),
        ("//127.0.0.1:9000/x", Refused::Origin),
        ("127.0.0.1:9000/x", Refused::Origin),
        ("/x", Refused::Origin),
        ("", Refused::Origin),
        ("https://127.0.0.1", Refused::Origin),
        ("https://127.0.0.1:9000", Refused::NoPath),
        ("https://127.0.0.1:", Refused::NoPath),
        ("https://127.0.0.1:/x", Refused::Port),
        ("https://127.0.0.1:0/x", Refused::Port),
        ("https://127.0.0.1:65536/x", out_of_range("65536")),
        ("https://127.0.0.1:99999/x", out_of_range("99999")),
        ("https://127.0.0.1:123456/x", Refused::Port),
        ("https://127.0.0.1:09000/x", Refused::Port),
        ("https://127.0.0.1:00080/x", Refused::Port),
        ("https://127.0.0.1:+80/x", Refused::Port),
        ("https://127.0.0.1:-80/x", Refused::Port),
        ("https://127.0.0.1: 80/x", Refused::Bytes),
        ("https://127.0.0.1:80 /x", Refused::Bytes),
        ("https://127.0.0.1:9000/x y", Refused::Bytes),
        ("https://127.0.0.1:9000/x\r\nHost: a", Refused::Bytes),
        ("https://127.0.0.1:9000/x#frag", Refused::Bytes),
        ("https://127.0.0.1:9000/\u{e9}", Refused::Bytes),
        ("https://127.0.0.1:9000/\0", Refused::Bytes),
    ];
    for (url, why) in table {
        assert_eq!(check(&origin(), url, pin()), Err(why.clone()), "{url:?}");
    }
}

#[test]
fn an_over_long_url_is_refused() {
    let at_limit = format!("https://127.0.0.1:9000/{}", "a".repeat(MAX_URL_LEN - 23));
    assert_eq!(at_limit.len(), MAX_URL_LEN);
    assert_eq!(accepted(&at_limit).port, 9000);
    let over = format!("{at_limit}a");
    assert_eq!(check(&origin(), &over, pin()), Err(Refused::TooLong));
}

#[test]
fn the_error_text_does_not_repeat_the_url() {
    let url = "https://secret-host.example/key?sig=hunter2";
    let error = check(&origin(), url, pin()).unwrap_err();
    assert!(!error.to_string().contains("hunter2"));
    assert!(!format!("{error:?}").contains("hunter2"));
}
