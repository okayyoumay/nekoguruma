# Fuzz targets

Fuzzing against malformed IR (design 12.1) with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz),
which needs a nightly toolchain. This directory is its own Cargo workspace, so the main
workspace's builds, tests and checks leave it out.

| Target | Input | What must hold |
|---|---|---|
| `ngr-fuzz-ecu-document` | Arbitrary bytes as a declaration part | `diag_ir::declaration::load_ecu_document` refuses malformed input with an error; a document it accepts unpacks in full, and its positive responses decode (the input standing in for a response) without a panic |
| `ngr-fuzz-vm` | A postcard-encoded `(Program, VmState)`, as a journal holds them | `Program::validate` and `Vm::check_state` return; a state that passes runs for up to 10,000 steps against a host that answers every primitive, without a panic or unbounded memory |

`seeds` writes the starting corpora: the documents of `diag-frontend`'s example definition, and
a few small programs with their initial states. They are generated, not committed, so they
follow the current schema.

```sh
cargo run --locked --manifest-path fuzz/Cargo.toml -p ngr-fuzz-seeds -- fuzz/corpus
cd fuzz
cargo +nightly fuzz run ngr-fuzz-ecu-document corpus/ngr-fuzz-ecu-document -- -max_total_time=60
cargo +nightly fuzz run ngr-fuzz-vm corpus/ngr-fuzz-vm -- -max_total_time=60 -rss_limit_mb=2048
```

CI's `fuzz` job runs both targets for 60 seconds each, on main only (`.github/workflows/ci.yml`). A crash input lands in
`fuzz/artifacts/<target>/`; fix the cause and add the input as a regression test in the crate
it reaches (`crates/diag-ir`).
