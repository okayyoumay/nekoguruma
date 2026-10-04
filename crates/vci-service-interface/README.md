# vci-service-interface

This crate defines the gRPC interface for the VCI service and generates the Rust bindings from `src/proto/service.proto`.

## Protocol Rules

See [docs/proto-rules.md](docs/proto-rules.md) for the naming, field-definition, and representation rules used by `service.proto`.

## Source of Truth

- Proto schema: [src/proto/service.proto](src/proto/service.proto)
- Generated bindings: [src/bindings/vci.service.rs](src/bindings/vci.service.rs)

## Implementation Note

See [docs/implementation-notes.md](docs/implementation-notes.md) for assumptions, implementation policy, checklist, and prioritized backlog.
