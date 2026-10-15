//! Frontends that turn a supply format into the IR (design 8.4). For the proprietary
//! vehicle-knowledge format (CSV tables and Starlark procedures, ADR-254, ADR-295), [`tables`]
//! reads a definition directory's CSV tables (`docs/csv-tables.md`), [`convert`] turns the
//! checked tables into the IR declaration part, and [`star`] checks a Starlark procedure against
//! the subset (`docs/starlark-subset.md`).

pub mod convert;
pub mod star;
pub mod tables;
