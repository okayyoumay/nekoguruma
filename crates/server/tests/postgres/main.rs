//! The server's tests that need a database, or that run beside the ones that do (ADR-458): one
//! test binary, so the PostgreSQL helper in `support` links once and nextest names one binary.

#[cfg(debug_assertions)]
mod agent_connect;
mod agent_events;
#[cfg(debug_assertions)]
mod agent_register;
mod api_spec;
mod binary;
mod control;
mod downloads;
mod instruction_key;
mod job_expiry;
mod job_queue;
mod jobs;
mod local_users;
mod migrations;
mod object_server;
mod object_server_rustfs;
mod redeem;
mod registration;
mod serving;
mod session;
mod storage;
mod support;
mod uploads;
