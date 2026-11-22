//! The wire names of the server's answer proof (ADR-504, design 6.4 and 14.4).
//!
//! The request carries the agent's nonce in [`NONCE_HEADER`]; the answer carries the server's
//! signature in [`PROOF_HEADER`]. The transcript itself is `shared_crypto::answer`. This module
//! holds what the server and the agent must agree on besides it: the header names and, for each
//! route, the statuses whose answer is a result, so that is proved and, on the agent's side,
//! required to carry a proof (ADR-504 item 3). The statuses are the ones a route promises; any
//! other status is classed by its status as before. The server proves more than the table lists
//! on the data routes (every answer after the token and the agent's row are judged), the agent
//! requires a proof for the listed statuses only.

/// The request header that carries the agent's 32-byte nonce, standard padded base64. Lower case,
/// the form HTTP libraries compare in.
pub const NONCE_HEADER: &str = "ngr-nonce";

/// The answer header that carries the server's 64-byte signature, standard padded base64.
pub const PROOF_HEADER: &str = "ngr-server-proof";

/// The session route's result: 200.
pub const SESSION_RESULTS: &[u16] = &[200];

/// The control upgrade's result: 101, with the proof as a header of the 101.
pub const UPGRADE_RESULTS: &[u16] = &[101];

/// The upload begin route's results: 200 (an upload that exists) and 201 (a new one).
pub const BEGIN_RESULTS: &[u16] = &[200, 201];

/// The part URL route's result: 200.
pub const PART_URL_RESULTS: &[u16] = &[200];

/// The upload status route's result: 200.
pub const STATUS_RESULTS: &[u16] = &[200];

/// The upload completion route's results: 200, 202 and 409.
pub const COMPLETE_RESULTS: &[u16] = &[200, 202, 409];

/// The download URL route's result: 200. Its other answers that end a fetch are taken only with a
/// valid proof too (ADR-504 item 3), which the agent's own table says.
pub const DOWNLOAD_RESULTS: &[u16] = &[200];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_names_are_lower_case_and_distinct() {
        for name in [NONCE_HEADER, PROOF_HEADER] {
            assert_eq!(name, name.to_ascii_lowercase());
            assert!(name.starts_with("ngr-"));
        }
        assert_ne!(NONCE_HEADER, PROOF_HEADER);
    }

    #[test]
    fn every_route_lists_its_statuses_once_and_none_is_a_refusal() {
        for table in [
            SESSION_RESULTS,
            UPGRADE_RESULTS,
            BEGIN_RESULTS,
            PART_URL_RESULTS,
            STATUS_RESULTS,
            COMPLETE_RESULTS,
            DOWNLOAD_RESULTS,
        ] {
            assert!(!table.is_empty());
            let mut sorted = table.to_vec();
            sorted.dedup();
            assert_eq!(sorted, table);
            // A 4xx is listed only where the route promises it (completion's 409).
            assert!(table.iter().all(|s| (100..=299).contains(s) || *s == 409));
        }
        assert_eq!(COMPLETE_RESULTS, &[200, 202, 409]);
        assert_eq!(BEGIN_RESULTS, &[200, 201]);
    }
}
