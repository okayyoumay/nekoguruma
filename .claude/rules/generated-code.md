---
paths:
  - "crates/iso22900-sys/**"
  - "crates/j2534-0404-sys/**"
  - "crates/vci-service-interface/**"
---

# Generated code

- **Proto bindings** (`crates/vci-service-interface`): after editing
  `src/proto/service.proto`, regenerate with
  `cargo build -p vci-service-interface --features vendored-protoc` and commit
  the result with the proto change. Update `docs/rpc-api-guide.md` in the same
  PR.
- **FFI bindings** (`crates/iso22900-sys`, `crates/j2534-0404-sys`): the C
  headers in `src/bindings/*.h` are the source of truth;
  `src/bindings/{target}.rs` is committed for every target listed in
  `docs/worker-crates.md` ("Target ABIs"). Never hand-edit a `{target}.rs`.
- Regenerate FFI bindings with
  `cargo build -p <crate> --features bindgen --target <target>`. This is slow
  and needs cross toolchains, so a header edit alone is not a request to
  regenerate: ask whether to regenerate now or defer. Never commit a header
  edit with stale bindings silently; record a deferral as an open item in
  `work/`.
- Without `--features bindgen` the build expects the committed `{target}.rs`
  and fails if it is missing, so adding a target means generating its file.
