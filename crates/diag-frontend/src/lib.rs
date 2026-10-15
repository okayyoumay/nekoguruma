//! Frontends that turn a supply format into the IR (design 8.4). For the proprietary
//! vehicle-knowledge format (CSV tables and Starlark procedures, ADR-254, ADR-295), [`tables`]
//! reads a definition directory's CSV tables (`docs/csv-tables.md`) and [`convert`] turns the
//! checked tables into the IR declaration part.

pub mod convert;
pub mod tables;
