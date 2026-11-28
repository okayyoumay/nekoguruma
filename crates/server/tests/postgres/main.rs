//! The server's tests that need a database, or that run beside the ones that do (ADR-458): one
//! test binary, so the PostgreSQL helper in `support` links once and nextest names one binary.

#[cfg(debug_assertions)]
mod agent_connect;
mod agent_events;
#[cfg(debug_assertions)]
mod agent_fetch;
#[cfg(debug_assertions)]
mod agent_register;
#[cfg(debug_assertions)]
mod agent_upload;
mod api_spec;
mod binary;
mod comm_log_requests;
mod comm_log_uploads;
mod control;
mod documents;
mod downloads;
mod instruction_key;
#[cfg(all(debug_assertions, unix))]
mod job_end_to_end;
mod job_expiry;
mod job_queue;
mod jobs;
mod local_users;
mod migrations;
mod object_server;
mod object_server_rustfs;
mod object_server_tls;
mod redeem;
mod registration;
mod serving;
mod session;
mod storage;
mod support;
mod uploads;
