//! Frontends that turn a supply format into the IR (design 8.4). For the proprietary
//! vehicle-knowledge format (CSV tables and Starlark procedures, ADR-254, ADR-295), [`tables`]
//! reads a definition directory's CSV tables (`docs/csv-tables.md`).

pub mod tables;
