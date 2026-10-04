# ADR-078: GetObjectId Returns PDU_ERR_INVALID_PARAMETERS When No Shortname Matches

**Date:** 2026-07-11
**Status:** Superseded by ADR-079 (for `OBJT_IO_CTRL`'s always-reject behavior
only, now superseded by `names.rs::map_ioctl_name`'s 17-command name table;
every other object type's "unrecognized shortname → `PDU_ERR_INVALID_PARAMETERS`"
behavior this ADR established, including `OBJT_IO_CTRL`'s own rejection of a
shortname matching none of those 17, is unaffected and stays `Accepted`)
**Affects:**
- `j2534-0404-service/src/service/names.rs`
- `j2534-0404-service/src/service/rpc_misc.rs`

## Context

`GetObjectId`'s resolver (`names.rs::resolve_object_id`) has one arm per
`ObjectType`. Before this ADR, the arms behaved inconsistently on a shortname
that did not resolve to anything meaningful:

- `OBJT_PROTOCOL` already rejected the request (`Status::not_found`) when the
  shortname matched no known protocol name *and* did not parse as a raw
  numeric `ChannelProtocol` value.
- `OBJT_BUSTYPE`, `OBJT_COMPARAM`, and `OBJT_PINTYPE` each tried a name lookup
  first, then fell back to `shortname.parse::<u32>().ok()`, and finally
  `.unwrap_or(0)` — so a typo'd or nonexistent shortname silently resolved to
  object ID `0`, indistinguishable from a real lookup that happens to return
  `0`.
- `OBJT_RESOURCE` was the same shape (`find_table_row_by_name`, then
  `map_protocol_name`, then `shortname.parse::<u32>().unwrap_or(0)`) — ADR-069
  explicitly retained and documented this `0`-fallback for unrecognized
  resource names ("unrecognized name keeps the pre-existing
  numeric-fallback/`0` behavior").
- `OBJT_IO_CTRL` had no name table at all: its entire resolution *was*
  `shortname.parse::<u32>().unwrap_or(0)`.

A caller that mistyped a shortname, or queried an object type/name
combination that genuinely does not exist, got back a successful response
with `pdu_object_id = 0` and no way to distinguish that from a legitimate
lookup — the D-PDU API defines `PDU_ERR_INVALID_PARAMETERS` for exactly this
class of caller error, but `GetObjectId` never returned it.

## Decision

**An unrecognized shortname is now rejected with `Status::invalid_argument`
carrying a `PDU_ERR_INVALID_PARAMETERS: ...` message** (the same
"`PDU_ERR_<NAME>`: description" convention `rpc_primitive.rs` already uses for
`PDU_ERR_TEMPPARAM_NOT_ALLOWED`), for every `ObjectType` except
`OBJT_PROTOCOL`:

- **`OBJT_BUSTYPE` / `OBJT_COMPARAM` / `OBJT_PINTYPE` / `OBJT_RESOURCE`:** the
  trailing `shortname.parse::<u32>().unwrap_or(0)` (or
  `.or_else(|| shortname.parse().ok()).unwrap_or(0)`) fallback branch is
  removed entirely. Only a name recognized by that type's own lookup
  (`map_bustype_name`/`map_comparam_name`/`map_pintype_name`/
  `find_table_row_by_name`, each still falling back to `map_protocol_name`'s
  legacy alias mapping exactly as before, where that path already existed)
  resolves; anything else is `PDU_ERR_INVALID_PARAMETERS`. A bare numeric
  shortname (e.g. `"31"`) that is not itself a name in the table no longer
  round-trips to that same number — it is rejected like any other
  unrecognized string. This supersedes the `0`-fallback behavior ADR-069
  documented for `OBJT_RESOURCE`; ADR-069's text is amended with a pointer to
  this ADR rather than marked wholesale superseded, since the rest of its
  resource-table decision is unaffected.
- **`OBJT_IO_CTRL`** had no name table at all — its entire resolution was the
  now-removed numeric fallback. `GetObjectId(OBJT_IO_CTRL, ...)` therefore
  unconditionally returns `PDU_ERR_INVALID_PARAMETERS` after this change.
  `rpc_misc.rs::rpc_io_ctl`'s IOCTL-by-name rejection message, which
  previously told callers to "use the numeric id from `GetObjectId`," is
  corrected since that path no longer exists.
- **`OBJT_PROTOCOL` is unchanged**, both in behavior and error style
  (`Status::not_found("unknown protocol shortname")`, no `PDU_ERR_` message
  prefix). It already rejected rather than defaulted, and its own numeric
  fallback is semantically different from the other five types': a bare
  numeric string there is a legitimate raw `ChannelProtocol` value (the same
  numeric space `CreateComLogicalLink`'s legacy fallback accepts), not an
  arbitrary passthrough of whatever number the caller happened to type.
  Aligning its status code/message style to the other five types was
  considered but is explicitly out of scope for this decision.

## Consequences

- **`GetObjectId` behavior change for five object types.** Any caller that
  relied on an unrecognized `OBJT_BUSTYPE`/`OBJT_IO_CTRL`/`OBJT_COMPARAM`/
  `OBJT_PINTYPE`/`OBJT_RESOURCE` shortname silently returning `0` must be
  updated to handle a `PDU_ERR_INVALID_PARAMETERS` error instead.
  `OBJT_IO_CTRL` in particular can no longer return a successful response for
  any input, since it has no name table to resolve against.
- **Tests updated in `names.rs`:** the comparam test asserting the old
  numeric-fallback/`0` behavior
  (`resolve_object_id_objt_comparam_keeps_numeric_fallback_and_unknown_zero`)
  is renamed and rewritten to
  `resolve_object_id_objt_comparam_rejects_unrecognized_shortname`; the
  bustype, pintype, and resource tests' trailing numeric-fallback assertions
  are replaced with `PDU_ERR_INVALID_PARAMETERS` rejection assertions; a new
  `resolve_object_id_objt_io_ctrl_always_rejects` test pins the
  always-rejects behavior for `OBJT_IO_CTRL`.
- **ADR-069 amended, not superseded.** Its "unrecognized name keeps the
  pre-existing numeric-fallback/`0` behavior" statements (Decision and
  Consequences sections) now point to this ADR; ADR-069's own `Status` line
  stays `Accepted` since the resource-table design itself is unchanged — only
  the `GetObjectId` unrecognized-name fallback it happened to document is
  superseded, mirroring how ADR-069 itself points to ADR-070 for a narrower
  textual supersession without flipping its own status.
- **Verification against the 2022 edition (2026-08-07, `iso22900-2-conformance-audit.md`
  finding A3-3).** The spec text this ADR's rejection choice was drawn from is internally in
  tension: `GetObjectId`'s Parameters clause describes the output parameter being set to
  `PDU_ID_UNDEF` when no object id is valid for the requested type/shortname (implying a
  success path for an unmatched name), while the same function's Return values table lists
  `PDU_ERR_INVALID_PARAMETERS` for an invalid `ShortName` (implying rejection) — both readings
  are textually valid in the 2009 edition, and neither clause cross-references or takes
  precedence over the other. Checked against the 2022 edition: the same two readings coexist
  unchanged (only the clause numbers shifted, 9.4.23.x → 8.4.23.x) — the newer edition does not
  resolve the tension either way. This ADR's choice (explicit rejection, favoring the return
  code table's enumeration) is retained as the deliberate, settled interpretation; it was not
  an oversight and does not need reconsideration on this basis.
