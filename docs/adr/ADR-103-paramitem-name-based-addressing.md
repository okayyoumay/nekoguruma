# ADR-103: ParamItem Name-Based Addressing for Set-Family RPCs

**Date:** 2026-07-19
**Status:** Accepted
**Affects:** `vci-service-interface/src/proto/service.proto`, `iso22900-service/src/service/convert.rs`, `iso22900-service/src/service/rpc_link.rs`, `iso22900-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_link.rs`, `j2534-0404-service/src/service/rpc_misc.rs`

## Context

`GetComParamRequest` already let a caller address a ComParam by either a
numeric `param_id` or a `param_name` string (`oneof param`), resolved
server-side (`PDUGetObjectId`-equivalent lookup). `ParamItem` — the message
used to both report a ComParam's value (`ComParamResponse`,
`UniqueRespIdTableResponse`) and to write one (`SetComParamRequest`,
`SetUniqueRespIdTableRequest`) — had no equivalent: it carried a bare
`uint32 com_param_id`, so every write path required the caller to already
know the numeric id, unlike the read path.

## Decision

`ParamItem.com_param_id` becomes `oneof id { uint32 param_id = 1; string
param_name = 8; }` — `param_id` keeps tag 1, and `param_name` takes the
next unused tag rather than displacing any existing field. `com_param_class`
(2) and the `param_data` oneof (3-7) keep their original tags unchanged, so
a client compiled against the pre-existing schema still decodes correctly
(`vci-service-interface/docs/implementation-notes.md`'s "preserve backward
compatibility for field tags and message semantics"; an initial version of
this change renumbered `com_param_class`/`param_data` to 3 and 4-8, which
would have silently corrupted an old client's `SetComParam`/
`SetUniqueRespIdTable` request — caught in Codex review before merge).

`ParamItem` remains a single message shared by both directions rather than
being split into separate read/write types — the entry-count blast radius
of splitting `ECUUniqueRespData`/`UniqueRespIdTableItem` (also shared
between `SetUniqueRespIdTableRequest` and `UniqueRespIdTableResponse`) into
input/output variants was judged disproportionate to the goal. A
server-produced `ParamItem` always sets `param_id`, never `param_name` —
this is a documented convention in the proto comment, not a wire-level
constraint.

**Name resolution is CLL-independent.** The same `param_name` always
resolves to the same `param_id`, regardless of which `ComLogicalLink` (and
therefore which protocol) it is used on. Whether the resolved id is
actually usable on a given CLL is a separate, protocol-dependent question,
answered after resolution by the pre-existing per-service mechanism
(`j2534-0404-service::comparam_support::check_param_allowed`; for
`iso22900-service`, whatever the connected vendor DLL's `PDUSetComParam`/
`PDUGetComParam` itself rejects). This separation matters in practice — see
ADR-104 for a case where the previous design conflated the two.

An unresolved `param_name` is rejected with `invalid_argument`.
`j2534-0404-service`'s pre-existing `GetComParam` name resolution used to
fall back to parsing the name as a bare number, then to id `0`, when the
name table had no entry (`.or_else(|| name.parse::<u32>().ok())
.unwrap_or(0)`); a bad read silently returning a wrong/zero value is
already undesirable, but a bad write silently targeting ComParam id `0`
instead of the intended one is worse, so this fallback is removed for both
`GetComParam` and the new `SetComParam`/`SetUniqueRespIdTable` name
resolution — both now share `names.rs::resolve_comparam_name`, which
returns `Err` on an unrecognized name.

**Applies to both `SetComParam` and `SetUniqueRespIdTable`.** Both accept
`ParamItem`s directly (`SetComParamRequest.param_item`,
`SetUniqueRespIdTableRequest.unique_resp_id_table.unique_data[].params`),
and both have a `cll_handle` in scope wherever resolution needs to happen,
so neither loses CLL context by resolving names.

**`iso22900-service`-specific behaviors**, since it is a thin real-DLL
passthrough rather than an adapter that models ComParam state itself:

- `rpc_set_com_param`: the real D-PDU API expects a concrete
  `T_PDU_PC` class; `PDU_PC_SPECIFIED` (the "unspecified" sentinel) is
  unconditionally rejected by `to_iso_param`'s class mapping, so there is
  no "let the sentinel through" fallback available. When the caller leaves
  `com_param_class` as `PDU_PC_SPECIFIED`, the service calls `GetComParam`
  first and uses the returned class in place of the sentinel before
  calling `PDUSetComParam`. If that `GetComParam` itself fails (e.g. the
  param was never read/set on this CLL), `SetComParam` fails with that
  `GetComParam` call's own error, not `to_iso_param`'s generic "not a
  concrete class" rejection — the auto-fill either succeeds or its
  real failure reason is surfaced (edge-case-review fix: an earlier
  version silently swallowed the `GetComParam` failure and let the
  sentinel reach `to_iso_param` anyway, where it was rejected regardless,
  just with a less informative error).
- `rpc_set_unique_resp_id_table`: a bare pass-through to
  `PDUSetUniqueRespIdTable` (full replace) — see the Consequences section
  below for why a merge-based alternative was tried and reverted.

## Consequences

- Every `ParamItem` construction/consumption site across
  `vci-service-interface`, `iso22900-service`, and `j2534-0404-service`
  (production and test code) needed updating for the new `id` oneof wrapper
  (the bare `com_param_id: u32` field became `id: Option<param_item::Id>`,
  a Rust-level type change even though the wire tags themselves did not
  move); `j2534-0500-service`'s `GetComParam`/`SetComParam`/
  `SetUniqueRespIdTable` handlers are still `not_yet_implemented` stubs and
  were unaffected.
- `resolve_param_item_id` (`iso22900-service::convert`) and
  `names.rs::resolve_comparam_name` (`j2534-0404-service`) are the two
  places name resolution actually happens; both are intentionally free of
  any CLL/protocol parameter.
- **`SetUniqueRespIdTable` is a full replace, not a merge — a read-merge-write
  design was tried and reverted (Codex review, PR #106).** The original
  concern (some D-PDU implementations drop the CLL-creation-time default
  entries for any `unique_resp_identifier` a partial-update request doesn't
  mention, since `PDUSetUniqueRespIdTable` can be a full replace rather than
  a merge on those implementations) is real, but always merging the
  request's entries over the existing table made it impossible to ever
  *remove* an entry through this RPC — the only table-mutation RPC that
  exists — since an omitted identifier would just be silently kept forever.
  That is a worse failure mode than the one being guarded against (e.g. an
  ECU permanently un-removable after a reconfiguration that drops it).
  `iso22900-service`'s `SetUniqueRespIdTable` is therefore a bare
  pass-through to `PDUSetUniqueRespIdTable`, matching `j2534-0404-service`'s
  own full-replace behavior for this table. The original vendor-quirk risk
  is now a caller-facing responsibility, documented in
  `docs/rpc-api-guide.md`'s "Unique Response ID Table" section: a caller
  doing a partial update should `GetUniqueRespIdTable` first and send back
  the complete desired table, not just the delta.
- `iso22900-mock`'s `PDUGetObjectId` previously returned success with id
  `0` for any unrecognized (object type, shortname) pair; it now returns
  `PDU_ERR_INVALID_PARAMETERS`, matching the behavior a spec-conformant
  vendor DLL is expected to have and matching what this ADR's name
  resolution needs to be exercisable in tests. No existing test relied on
  the old lenient behavior.
