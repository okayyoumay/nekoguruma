//! The operational log's file output (design 12.2, ADR-348 item 5).
//!
//! - [`RotatingFile`]: a private log file, rotated by size, the oldest rotated file deleted
//!   beyond a count
//! - [`JsonLayer`]: a `tracing` layer that writes each event as one JSON object per line
//!
//! The caller installs the layer with its own filters and decides what to do when the file
//! cannot be opened privately ([`OpenError`]); ADR-348 item 5 has it fall back to stderr.
//! `docs/file-output.md` describes the format and the settings.

mod file;
mod json;
mod private;
mod time;

pub use file::{FileSettings, OpenError, RotatingFile};
pub use json::JsonLayer;
