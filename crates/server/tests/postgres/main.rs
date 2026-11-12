//! The server's tests that need a database, or that run beside the ones that do (ADR-458): one
//! test binary, so the PostgreSQL helper in `support` links once and nextest names one binary.

mod binary;
mod migrations;
mod serving;
mod support;
