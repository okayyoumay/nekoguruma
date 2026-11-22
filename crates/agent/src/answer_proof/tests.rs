//! The verifier against a prover that signs as the server does: what is proved and every way an
//! answer fails to be.

use std::sync::Arc;

use shared_crypto::SigningKey;

use super::*;

const AGENT_ID: &str = "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4e";
const ORIGIN: &str = "http://127.0.0.1:8080";
const REQUEST: &[u8] = br#"{"agentId":"x"}"#;
const ANSWER: &[u8] = br#"{"token":"t","expiresInSecs":5}"#;

struct Pair {
    verifier: Verifier,
    prover: TestProver,
}

fn pair() -> Pair {
    let key = Arc::new(SigningKey::generate().unwrap());
    let audience = Audience::parse_canonical(ORIGIN).unwrap();
    Pair {
        verifier: Verifier {
            key: shared_crypto::Signer::public_key(&*key),
            audience: audience.clone(),
            agent_id: AGENT_ID.to_owned(),
        },
        prover: TestProver {
            key,
            audience,
            agent_id: AGENT_ID.to_owned(),
        },
    }
}

fn sent(nonce: [u8; 32]) -> Sent<'static> {
    Sent {
        nonce,
        method: "POST",
        target: "/api/v1/agent/session",
        body: REQUEST,
    }
}

fn proof_of(pair: &Pair, nonce: [u8; 32]) -> String {
    pair.prover
        .proof(nonce, "POST", "/api/v1/agent/session", REQUEST, 200, ANSWER)
}

#[test]
fn a_proof_of_this_request_and_answer_is_proved() {
    let p = pair();
    let nonce = shared_crypto::random_32().unwrap();
    let proof = proof_of(&p, nonce);
    assert!(p.verifier.proved(&sent(nonce), 200, ANSWER, Some(&proof)));
}

#[test]
fn every_way_an_answer_differs_from_the_one_proved_is_not_proved() {
    let p = pair();
    let nonce = shared_crypto::random_32().unwrap();
    let proof = proof_of(&p, nonce);
    let other = pair();
    let other_nonce = shared_crypto::random_32().unwrap();
    let another_agent = Verifier {
        key: p.verifier.key,
        audience: p.verifier.audience.clone(),
        agent_id: "0194f0a2-7c3e-7d11-8a52-3f6f9b1c2d4f".to_owned(),
    };
    let another_audience = Verifier {
        key: p.verifier.key,
        audience: Audience::parse_canonical("http://127.0.0.1:8081").unwrap(),
        agent_id: AGENT_ID.to_owned(),
    };
    let cases: Vec<(&str, bool)> = vec![
        (
            "missing",
            p.verifier.proved(&sent(nonce), 200, ANSWER, None),
        ),
        (
            "another key",
            other
                .verifier
                .proved(&sent(nonce), 200, ANSWER, Some(&proof)),
        ),
        (
            "another nonce",
            p.verifier
                .proved(&sent(other_nonce), 200, ANSWER, Some(&proof)),
        ),
        (
            "another agent ID",
            another_agent.proved(&sent(nonce), 200, ANSWER, Some(&proof)),
        ),
        (
            "another audience",
            another_audience.proved(&sent(nonce), 200, ANSWER, Some(&proof)),
        ),
        (
            "body altered",
            p.verifier
                .proved(&sent(nonce), 200, b"{\"token\":\"u\"}", Some(&proof)),
        ),
        (
            "status altered",
            p.verifier.proved(&sent(nonce), 202, ANSWER, Some(&proof)),
        ),
        (
            "another method",
            p.verifier.proved(
                &Sent {
                    method: "GET",
                    ..sent(nonce)
                },
                200,
                ANSWER,
                Some(&proof),
            ),
        ),
        (
            "another target",
            p.verifier.proved(
                &Sent {
                    target: "/api/v1/agent/control",
                    ..sent(nonce)
                },
                200,
                ANSWER,
                Some(&proof),
            ),
        ),
        (
            "another request body",
            p.verifier.proved(
                &Sent {
                    body: b"{}",
                    ..sent(nonce)
                },
                200,
                ANSWER,
                Some(&proof),
            ),
        ),
    ];
    for (name, proved) in cases {
        assert!(!proved, "{name} must not be proved");
    }
}

#[test]
fn a_proof_that_is_not_canonical_base64_of_64_bytes_is_not_proved() {
    let p = pair();
    let nonce = shared_crypto::random_32().unwrap();
    let proof = proof_of(&p, nonce);
    let url_safe = proof.replace('+', "-").replace('/', "_");
    let cases = [
        ("empty", String::new()),
        ("short", proof[..proof.len() - 4].to_owned()),
        ("long", format!("{proof}AAAA")),
        ("unpadded", proof.trim_end_matches('=').to_owned()),
        ("padded with spaces", format!(" {proof}")),
        ("another alphabet", url_safe),
        ("not base64", "!".repeat(proof.len())),
    ];
    for (name, text) in cases {
        // A url-safe copy of a proof with neither character is the proof itself.
        if text == proof {
            continue;
        }
        assert!(
            !p.verifier.proved(&sent(nonce), 200, ANSWER, Some(&text)),
            "{name} must not be proved"
        );
    }
}

#[test]
fn a_signature_made_of_filler_bytes_is_not_proved() {
    let p = pair();
    let nonce = shared_crypto::random_32().unwrap();
    let proof = proof_of(&p, nonce);
    let mut bytes: [u8; 64] = canonical_base64::<64>(&proof).unwrap();
    bytes[32..].fill(0xff);
    let text = STANDARD.encode(bytes);
    assert!(!p.verifier.proved(&sent(nonce), 200, ANSWER, Some(&text)));
}
