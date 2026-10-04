---
paths:
  - "crates/iso22900*/**"
  - "crates/j2534-0404*/**"
  - "crates/vci-service-*/**"
  - "docs/worker-crates.md"
  - "docs/rpc-api-guide.md"
  - "docs/j2534-*.md"
---

# Worker crates

- Layering: `*-sys` (raw FFI) -> safe wrapper (`iso22900`, `j2534-0404`) ->
  `*-service` (gRPC worker binary). Keep unsafe code in `*-sys` and the safe
  wrappers; services call the wrappers.
- Design decisions are cited as `ADR-NNN`; check `docs/adr/INDEX.md` for the
  area before changing behaviour, and align with or explicitly supersede the
  ADR.
- Spec citations: cite ISO 22900-2 / SAE J2534 by clause number and
  paraphrase. Note which ISO 22900-2 edition (2009 or 2022) a statement
  targets.
- Existing comments and notes mention PR numbers, review rounds and agent
  names from these crates' earlier history. Read them as provenance; do not
  add new references of that kind.
- Tests: run a new test once in isolation and confirm the binary exits (a
  gRPC test that keeps an event stream open can hang at server shutdown).
  With `-- --exact`, pass the full module path and confirm a nonzero test
  count.
- The mocks (`iso22900-mock`, `j2534-0404-mock`) are cdylibs that tests
  load from the nearest `target/` directory; build them before running tests
  that need them.
