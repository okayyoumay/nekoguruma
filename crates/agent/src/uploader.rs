//! The agent's data channel client (design 5.2, 5.4, 13.3, ADR-493, ADR-495): the library the uploader's
//! loop is built from.
//!
//! - [`http`]: the HTTP/1 client for the four routes and the part PUT: one request per
//!   connection, no redirect, a head and a JSON answer within bounds, a body streamed in chunks,
//!   and the no-progress clock of design 5.4
//! - [`part_url`]: the host rule a part URL must meet before the agent sends a byte to it
//!   (ADR-493 item 13)
//! - [`routes`]: the four routes' requests and strict answers, and how each answer is classified
//!   for the loop: deliver, retry, pass over, or exchange a new token
//!
//! Nothing here logs; the loop decides what is worth a line. A token, a URL and a body are never
//! put in an error.

pub mod http;
pub mod part_url;
pub mod routes;

/// The in-memory server the unit tests of this module and of the exchange's second caller use.
#[cfg(test)]
pub(crate) mod fake;
