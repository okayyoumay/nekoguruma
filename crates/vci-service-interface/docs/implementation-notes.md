# vci-service-interface Implementation Note

## Scope

Defines gRPC/protobuf contract and generated Rust bindings consumed by service and client-side integrations.

## Assumptions

- service.proto is the source of truth for wire contract.
- Generated bindings are checked into source and included directly by lib.rs.
- Regeneration remains feature-gated through protoc options.
- proto-rules.md is a normative naming and field-policy reference.
- `src/rich_error.rs` is hand-written, non-generated Rust and is not touched
  by the regeneration workflow below; it only depends on message types the
  regeneration step produces (`ErrorDetail`/`ErrorEventData`). See
  `docs/adr/ADR-105-rich-error-model-replaces-get-last-error.md`.

## Implementation Policy

- Preserve backward compatibility for field tags and message semantics.
- Treat presence-model changes (optional/oneof/singular) as compatibility-sensitive.
- Keep schema and regenerated bindings in the same change.
- Prefer vendored protoc for reproducible CI and cross-environment consistency.

## Proto Code Generation

Generated Rust bindings live in `src/bindings/vci.service.rs` and are
committed to source.  Regeneration is triggered manually by building with
one of two opt-in Cargo features:

### `protoc` feature

Uses the `protoc` binary already installed on the host.

```
cargo build -p vci-service-interface --features protoc
```

`build.rs` calls `tonic-prost-build` with the system-installed `protoc`
(`PROTOC` environment variable or `PATH` lookup).  The output is written
to `src/bindings/` in place.

### `vendored-protoc` feature

Downloads a pre-built `protoc` binary at build time via the
`protoc-bin-vendored` crate.  No local `protoc` installation required.

```
cargo build -p vci-service-interface --features vendored-protoc
```

`build.rs` obtains the binary path from `protoc_bin_vendored::protoc_bin_path()`
and passes it explicitly to `prost-build`, overriding any host `protoc`.

`vendored-protoc` implies `protoc`; enabling the former activates both.

### Feature dependency graph

```
vendored-protoc
  └─► protoc
        ├─► prost-build        (optional dep)
        └─► tonic-prost-build  (optional dep)
```

When neither feature is set, `build.rs` compiles to a no-op and the
committed bindings are used as-is.

### Regeneration workflow

1. Edit `src/proto/service.proto`.
2. Run `cargo build -p vci-service-interface --features vendored-protoc`.
3. Review `src/bindings/vci.service.rs` for correctness.
4. Fix up any compilation errors in dependent crates.
5. Commit proto source and generated bindings together.

## Change Checklist

1. Review wire compatibility impact before altering existing messages or enums.
2. Regenerate bindings and include resulting diff in the same update.
3. Validate server crate builds and key RPC tests after schema changes.
4. Update protocol rules or migration notes when exceptions are introduced.
