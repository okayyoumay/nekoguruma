# ADR-218: `iso22900-service` ComParam STRUCTFIELD Support — Standard Struct Types, and a Size-Resolution Algorithm for Vendor Struct Types

**Date:** 2026-09-04
**Status:** Accepted and implemented (2026-09-04, same-day follow-up — see Consequences); amended
2026-09-07 (Decision item 4's process-lifetime write cache removed; in separate PR #133 Codex
review rounds the same day, `vendor_struct_types` key-grammar startup validation added, standard
struct type ids (0x1-0x3) rejected from that same table, a zero-valued arch-level entry no longer
shadows a valid api-level fallback, startup validation switched to a strict config loader, and the
write-side buffer's alignment guarantee widened from 8 to 16 bytes, and Decision item 6's public
`to_owned_with_vendor_entry_size[_resolver]` entry points made `unsafe fn` (they were left safe by
mistake, reachable by arbitrary safe callers with an unverifiable size); amended 2026-09-08 (PR #133
Codex review round: a configured `vendor_struct_types` entry size is now bounded by a sanity
ceiling, mirroring ADR-219's identical `vendor_ioctls` fix; a same-day pre-merge edge-case-hunter
audit then found the initial startup-only enforcement insufficient and widened it to also re-check
on every per-request lookup, since unlike `vendor_ioctls` this table is re-read from disk on every
call) — see Consequences)
**Affects:** `iso22900-service/src/service/convert.rs`, `iso22900-service/src/service/rpc.rs`,
`iso22900-service/src/service/rpc_link.rs`, `iso22900-service/src/service/rpc_misc.rs`,
`iso22900/src/item/data/common.rs`, `iso22900/src/item/param.rs`,
`iso22900/src/item/unique_resp.rs`, `vci-service-config/src/lib.rs`,
`iso22900-service/docs/implementation-notes.md`, `docs/rpc-api-guide.md`,
`docs/worker-crates.md`, `docs/glossary.md`, ADR-178

## Context

This ADR is part of a broader effort to expose vendor-specific extensions of the native D-PDU
API / J2534 DLLs through the existing shared gRPC proto
(`vci-service-interface/src/proto/service.proto`), constrained by ADR-178, which freezes that
proto file against new messages/fields for J2534-2 features that can be re-expressed through a
mechanism already present at ADR-178's own baseline commit. ADR-178's Consequences section (its
final bullet, "Not addressed by this ADR") explicitly carves out one thing its Decision does not
cover: a genuinely new client-visible RPC-level semantic — a new stream or handle concept — since
its Decision only governs *messages and fields*, not *RPC methods*. This ADR introduces no new
RPC method, message, or field, so it stays entirely within ADR-178's frozen surface.

ISO 22900-2:2022 §B.3.3 (paraphrased) defines ComParam data types as scalars (UNUM8/16/32,
SNUM8/16/32), BYTEFIELD, LONGFIELD, and STRUCTFIELD — an array of same-sized struct entries whose
layout is selected by a `ComParamStructType` discriminant. Three struct types are standardized
(SESSION_TIMING, ACCESS_TIMING, TLS_VERSION_AND_CIPHER); any other discriminant value is a
vendor/tool-specific struct layout the spec does not define.

The gRPC proto already models this shape in full, unused today:
`vci-service-interface/src/proto/service.proto:256-263` defines `ParamStructfield` as a oneof
with three typed list variants (`session_timing`, `access_timing`, `tls_version_and_cipher`,
lines 216-247) plus a fourth, `vendor_specific` (`ParamVendorSpecificStruct`, lines 249-254):
`{ string type_url; uint32 size_of_entry; uint32 count_of_entry; bytes value; }`. This is wired
into `ParamItem.param_data` at tag 7 (`service.proto:297`).

The `iso22900` Rust crate already parses/encodes this end-to-end, independent of the gRPC layer:

- `ComParamValue::StructField { struct_type: T_PDU_CPST, encoding: StructFieldEncoding { entry_size,
  bytes } }` (`iso22900/src/lib.rs:277-280`, with `StructFieldEncoding` at lines 296-300).
- Encoding a `ComParamValue::StructField` for `PDUSetComParam` dispatches on `struct_type`:
  standard types build the typed struct array; any other value is treated as vendor-specific and
  requires `encoding.entry_size > 0` with `encoding.bytes.len()` a multiple of it
  (`iso22900/src/encode.rs:210-236`).
- Decoding a single `PDUGetComParam` result for a vendor struct type requires the caller to supply
  the entry size explicitly — `OwnedParamItem::from_borrowed_parts_impl(item, vendor_entry_size:
  Option<usize>)` returns `DPduApiError::Unsupported("vendor specific structfield clone requires
  explicit entry size")` when `vendor_entry_size` is `None` for a non-standard `struct_type`
  (`iso22900/src/item/param.rs:303-305, 348-361`).
- The same requirement exists for `GetUniqueRespIdTable`, whose multi-entry table can mix struct
  types across rows: `to_owned_with_vendor_entry_size_resolver` takes a
  `FnMut(T_PDU_CPST) -> Option<usize>` resolver instead of a single size
  (`iso22900/src/item/unique_resp.rs:89-101`).

The only gap is in `iso22900-service` itself: `iso22900-service/src/service/convert.rs` has three
`Status::unimplemented(...)` arms that reject every STRUCTFIELD ComParam outright, standard or
vendor — `read_borrowed_param_value`'s read-side arm (lines 77-81, shared by `GetComParam` and
`GetUniqueRespIdTable` per its own doc comment at lines 37-38), `to_iso_param`'s write-side arm
(lines 200-204), and `from_iso_param`'s read-serialize arm (lines 232-236).

**The read-side problem this ADR must solve.** For a vendor struct type, the native
`PDUGetComParam` call returns only an entry count (`ParamActEntries`) and an untyped `void
*pStructArray` (`iso22900-sys/src/bindings/d_pdu_api_defs.h:56-61`) — the entry byte-size is not
recoverable from the D-PDU API call itself, confirmed by `iso22900/src/item/param.rs:357-359`'s
explicit error when no size is supplied. Decoding a vendor STRUCTFIELD read therefore requires
`iso22900-service` to already know the entry size from somewhere else before it can even ask the
`iso22900` crate to decode the payload.

`vci-service-config` already has a three-level per-library configuration precedent this ADR
extends: `InstanceConfig` (`vci-service-config/src/lib.rs:170-184`) carries `library_path`,
`can_channel_mode`, and `modules`, looked up via `[config.apis.<api>.arch.<arch>.libs.<lib>]` ->
`[config.apis.<api>.libs.<lib>]` precedence (documented per-accessor, e.g. lines 419-421,
479-484).

`iso22900-service` loads exactly one D-PDU library per OS process: the startup argument names a
single `<library name>` and "the process owns one local gRPC server instance"
(`iso22900-service/docs/grpc-instance-spec.md` lines 13-16, 35), and
`vci-service-manager::spawn_instance` is described in ADR-032's own Context (line 9) as launching
"one gRPC service process per requested library." Combined, these establish that within one
`iso22900-service` process lifetime, "the connected library" is a fixed, singular fact — so a
process-lifetime cache keyed only by `struct_type` (not also by library identity) cannot
cross-contaminate between different libraries the way a longer-lived cache might.

**Amendment (2026-09-07): the process-lifetime write cache below (originally Decision item 4b) is
a local heap-disclosure vector, found by Codex review.** The reasoning above about cross-library
contamination is sound as far as it goes, but it assumes a cached size, once learned, is
trustworthy — that assumption is the actual defect. A native `PDUSetComParam` call for a vendor
STRUCTFIELD value does not validate the client's declared `size_of_entry` against the connected
library's real per-entry layout at all: the DLL reads `count_of_entry * <its own true size>` bytes
regardless of what the client claimed, and reports success either way. Two consequences follow.
First, an *empty* write (`count_of_entry: 0`) reaches the native call harmlessly (zero entries to
read), but can carry an arbitrary `size_of_entry` — e.g. `0xFFFFFFFF` — that the pre-amendment cache
logic still recorded as long as it was nonzero, because only `entry_size == 0` was excluded from
caching; a subsequent read of the same struct type with a real nonzero `ParamActEntries` would then
construct an unchecked ~4 GiB slice from the native pointer. Second, even a *non-empty* write whose
declared size understates the DLL's true per-entry size "succeeds" (the DLL simply reads less than
its own real per-entry size implies), and that undersized value gets cached and later trusted for a
read that constructs a slice past the DLL's actual returned allocation. In both cases, a size
learned from a write is never more trustworthy than the request it came from — trusting it on a
later read builds an unchecked slice over a native allocation whose true length the D-PDU API never
exposes to this service at all. This is a local-client-to-service-process disclosure, not a
network-remote one (the gRPC listener is loopback-only per ADR-052), but it is a real vulnerability:
any local process able to reach the loopback listener could read out-of-bounds heap memory this way.
See Decision item 4 (as amended) and Consequences below for the fix.

## Decision

**1. Wire the existing `ParamStructfield` oneof into `iso22900-service`; remove the three
`unimplemented` arms in `convert.rs`.** No new proto messages or fields are introduced (satisfies
ADR-178).

**2. Standard struct types (`PDU_CPST_SESSION_TIMING`/`PDU_CPST_ACCESS_TIMING`/
`PDU_CPST_TLS_VERSION_AND_CIPHER`, values `0x00000001`-`0x00000003`) map 1:1, in both directions,
between the native D-PDU struct layout and the proto's three typed list variants**, using
`size_of::<PDU_PARAM_STRUCT_*>()` (whichever native struct corresponds to the discriminant) as the
entry size. No ambiguity, no configuration needed — the `iso22900` crate already does this
encode/decode step (`encode.rs:212-222`); `convert.rs` only needs to translate between the crate's
`ComParamValue::StructField` and the proto's `ParamSessionTimingList`/`ParamAccessTimingList`/
`ParamTlsVersionAndCipherList` shapes, one struct-field row at a time.

**3. Vendor struct types (any `ComParamStructType` value outside `0x1`-`0x3`) use the
`ParamVendorSpecificStruct` (`vendor_specific`) variant:**

- `type_url` carries the `ComParamStructType` discriminant under a documented grammar:
  `"pdu-cpst:0x<8 lowercase hex digits>"` (e.g. `"pdu-cpst:0x00000005"`). Mandatory on write; the
  service fills it in on read, from the native call's own struct-type context (the connected
  library already told the service which `struct_type` it asked for/received). Writing a `type_url`
  whose decoded value is `0x1`-`0x3` under this vendor path is rejected `invalid_argument` — there
  is exactly one canonical encoding per struct type (the typed variant for standard types, the
  vendor variant for everything else); accepting both would let the two drift for the same value.
- `value` (the `bytes` field) is the vendor's native in-memory struct layout **verbatim** — the
  client packs exactly what the vendor's own header/MDF describes. Per ISO 22900-2:2022 §B.3.3.2
  NOTE 3 (paraphrased): the spec guarantees only even-byte alignment for a struct entry, imposing
  and permitting no other re-encoding, so `iso22900-service` neither can nor should reinterpret
  these bytes.
- Write validation: reject `invalid_argument` unless `value.len() == size_of_entry *
  count_of_entry`, and unless `size_of_entry > 0` whenever `count_of_entry > 0` — mirroring the
  `iso22900` crate's own encode-side check (`encode.rs:224-233`) at the gRPC boundary, so a
  malformed request never reaches the native call at all. A non-empty write is also subject to
  Decision item 4's entry-size resolution below, which can additionally reject it
  `failed_precondition`/`invalid_argument`.

**4. Entry-size resolution (as amended 2026-09-07) — the crux of this ADR, and now identical for
both directions.** Operator config is the ONLY source of a vendor struct type's entry size, for
`GetComParam`/`GetUniqueRespIdTable` (read) and `SetComParam`/`SetUniqueRespIdTable` (write) alike.
There is no process-lifetime write cache (originally item 4b below; removed — see the Amendment
note in Context and Consequences for why).

   a. **Per-library operator config, the sole source.** Extend `vci-service-config`'s
      `InstanceConfig` (`vci-service-config/src/lib.rs:170-184`) with a new field,
      `vendor_struct_types: Option<HashMap<String, u32>>`, populated from a new
      `[config.apis.iso22900.libs."<lib>".vendor_struct_types]` table (and its
      `arch.<arch>.libs.<lib>` counterpart, following the exact same two-level precedence
      (arch+lib, then api+lib, no api-level or root-level fallback) `find_library_path`
      (`vci-service-config/src/lib.rs:474-489`) already implements for the identical reason: a
      per-struct-type entry size only makes sense tied to a specific library. Each key is the same
      `"0x<8 lowercase hex digits>"` string used in `type_url`'s suffix; each value is the entry
      size in bytes. A new accessor, `find_vendor_struct_type_size(api, arch, library_name,
      struct_type: u32) -> Option<u32>`, mirrors the existing accessors' shape. A configured `0` is
      treated the same as "not configured" (`0` is never a valid per-entry byte layout for a
      nonempty table/write).
   b. **Read: unconfigured -> `failed_precondition`, naming the struct type and the remedy
      (configure `vendor_struct_types` for this library — there is no second remedy anymore, since
      there is no cache for a prior write to satisfy).**
   c. **Write, non-empty (`count_of_entry > 0`): unconfigured -> `failed_precondition`, identical
      to (b); configured -> the request's `size_of_entry` MUST equal the configured value exactly,
      or reject `invalid_argument` naming both the declared and configured sizes.** This is the
      fix: a successful native `PDUSetComParam` never validates the caller's declared size against
      the library's real layout, so the service must, or nothing ever does.
   d. **`ParamActEntries == 0` (read) / `count_of_entry == 0` (write) needs no size resolution at
      all, in either direction** — a read reports `count_of_entry: 0, value: []` unconditionally; a
      write is accepted unconditionally, regardless of what `size_of_entry` claims. This is what
      closes the empty-write-poison exploit path described in the Amendment note above: an empty
      write's `size_of_entry` is never consulted for anything, on either side, so it has nothing
      left to poison.
   e. **Never fabricate `size_of_entry: 0` (or any other value) for a struct type that actually has
      a nonzero `ParamActEntries`.** `size_of_entry: 0` alongside a real, nonzero count would look
      like a legitimate configured value to a client while actually carrying no size information —
      the same "silently-looks-like-a-value" failure mode ADR-133 identifies and closes for
      `j2534-0404-service`'s unseeded Bytefield/Structfield `GetComParam` fallback (a different
      package's ADR, cited here only as the same underlying design principle: an unresolved value
      must fail loudly, not report a fabricated-looking default). This ADR states that same
      principle directly as its own reasoning for `iso22900-service`, per rule (b)/(c) above.

**5. `GetComParam`/`GetUniqueRespIdTable` (read) and `SetComParam`/`SetUniqueRespIdTable` (write)
all share this resolution logic** — extended 2026-09-07 to cover the write path, which originally
had no shared resolver at all. Both read paths funnel through `convert.rs`'s
`read_borrowed_param_value` (its own doc comment already states this, lines 37-38). For a
single-value `GetComParam` read, the resolved size feeds `OwnedParamItem::from_borrowed_parts_impl`'s
`vendor_entry_size: Option<usize>` parameter (`iso22900/src/item/param.rs:303-305`) directly. For a
`GetUniqueRespIdTable` read, whose table can mix struct types across rows, the same resolution steps
are wrapped in a `FnMut(T_PDU_CPST) -> Option<usize>` closure fed to
`to_owned_with_vendor_entry_size_resolver` (`iso22900/src/item/unique_resp.rs:89-101`) instead of
resolving once. On write, `convert.rs`'s `to_iso_param`/`structfield_from_proto`/
`vendor_struct_from_proto` (`SetComParam`, via `rpc_link.rs`) and `from_unique_response_item`
(`SetUniqueRespIdTable`, via `rpc_misc.rs`) take the identical `&mut dyn FnMut(E_PDU_CPST) ->
Result<u32, Status>` resolver shape `read_borrowed_param_value` already used, both call sites backed
by the same `Iso22900Service::resolve_vendor_struct_entry_size` method (`rpc.rs`) the read paths
call too — one shared resolution function, called once per size lookup, so the algorithm itself
cannot drift between any of these call sites, read or write.

**6. The raw vendor-slice helpers in the `iso22900` crate are `unsafe fn` (2026-09-07 amendment).**
No bounds check is constructible against a length the D-PDU API never exposes — the fix is closing
the untrusted *source* of the size (item 4 above), not adding a check that cannot actually be
written. `iso22900/src/item/data/common.rs`'s `BorrowedVendorSpecificStructArray::bytes` and
`BorrowedStructfieldData::struct_array_bytes_with_entry_size` are `unsafe fn`, each with a `#
Safety` doc comment stating the caller must supply the connected library's TRUE per-entry byte
layout from trusted (operator-configured) information — an incorrect size is undefined behavior.
The standard (non-vendor) struct-type read call site in `convert.rs` uses the existing, already-safe
`struct_array_bytes()` instead (it never needed a caller-supplied size to begin with).
`OwnedParamItem::from_borrowed_parts_impl` (`pub(crate)`, called only by this crate's own trusted,
resolver-driven call sites) keeps a safe signature — `unsafe` is scoped no wider than necessary,
not the whole call chain above the two lowest-level functions — but gains a documented, uncheckable
precondition in its own doc comment instead.

**Amended 2026-09-07, PR #133 Codex review round (correcting this item's scope: the three
genuinely `pub fn` entry points also need `unsafe`).** This item's original text kept
`to_owned_with_vendor_entry_size_resolver` safe alongside `from_borrowed_parts_impl`, reasoning
that `unsafe` need not extend past the two lowest-level slice-construction functions. That
reasoning holds for `pub(crate)` functions reachable only from this crate's own trusted,
config-sourced call sites (`from_borrowed_parts_impl`, `from_borrowed_with_vendor_entry_size[_resolver]`)
— but `BorrowedParamItem::to_owned_with_vendor_entry_size` (`param.rs`) and
`BorrowedUniqueRespIdTableItem::to_owned_with_vendor_entry_size`/
`to_owned_with_vendor_entry_size_resolver` (`unique_resp.rs`) are `pub fn`, part of the `iso22900`
crate's actual public API surface, reachable by ANY safe caller with an arbitrary
`vendor_entry_size`/resolver — not just this crate's own trusted internal callers. A safe function
that cannot validate its precondition against the native allocation's real size, yet can be called
by arbitrary safe code with a value that makes it perform an out-of-bounds read, is unsound
regardless of what its doc comment says; a doc comment is not enforced by the compiler the way
`unsafe fn` is. All three are now `pub unsafe fn` with a `# Safety` section identical in substance
to the two lowest-level functions' existing ones. `pub(crate)` helpers callable only from this
crate's own trusted call sites are unaffected and remain safe-with-documented-precondition, per
this item's original reasoning — that reasoning was correct for them, just not for the wider `pub
fn` surface. Test call sites in `unique_resp.rs` wrapped in `unsafe { }` blocks accordingly; no
other crate in the workspace calls any of these three functions.

**7. The IOCTL side needs no design change — confirmed and documented as-is.**
`IoCtrlCommandName` already resolves manufacturer-specific IOCTL names via
`PDUGetObjectId(OBJT_IO_CTRL, ...)` (`iso22900-service/src/service/rpc_misc.rs:28-30`), matching
how ISO 22900-2:2022 §8.5.1 (paraphrased) describes manufacturer IOCTLs as reached by short name
via the vendor's own MDF, not by a numeric ID range convention the way J2534's IoctlID space works.
IOCTL's own data-type system, `E_PDU_IT` (`iso22900-sys/src/bindings/d_pdu_api_defs.h:94-114`), is
a closed enum with no STRUCTFIELD-like complex/vendor-discriminated type, and every one of its
members already maps 1:1 onto the proto's `DataItem` oneof. There is nothing to add: a vendor
IOCTL needing a data shape the D-PDU API itself has no `E_PDU_IT` member for is impossible to
express at the native API level, not a gRPC-layer gap — recorded as an accepted residual, not a
follow-up.

### Alternatives Considered

- **An ad-hoc byte-packing convention inside an existing scalar/bytes field, instead of using
  `ParamStructfield.vendor_specific`.** Rejected: the proto already carries a typed, self-describing
  vendor escape hatch for exactly this case (`type_url`/`size_of_entry`/`count_of_entry`/`bytes`) —
  inventing a second encoding for the same value would create drift between two routes to the same
  data, with no benefit.
- **`google.protobuf.Any` in place of `ParamVendorSpecificStruct`.** Rejected: there is no
  self-description problem here to solve — `type_url`+`size_of_entry`+`count_of_entry`+`bytes`
  already fully self-describes a vendor struct entry — and `Any` would pull well-known-types
  machinery into a proto file ADR-178 explicitly freezes against new growth for a case that does
  not need it. (Note: ADR-178 itself does not discuss `Any` anywhere — this rejection is this
  ADR's own reasoning, not a claim about ADR-178's text.)
- **Inferring the struct-type discriminant from `param_id`/`param_name` instead of requiring the
  client to state it via `type_url`.** Rejected: `iso22900-service` has no MDF and no metadata call
  beyond `GetComParam` itself, which still would not recover the entry size — the actual scarce
  fact this design needs. Inferring the discriminant alone does not remove the client's real
  burden, it only moves it.

**Added 2026-09-07, evaluated when closing the write-cache heap-disclosure finding:**

- **Keeping the process-lifetime write cache write-only (never consulted on read).** Rejected: a
  cache no read path ever consults has no purpose at all, so this is equivalent to removing it —
  and it still would not have closed anything, since the vulnerability was never about the read
  side reading the cache, it was about the cache ever trusting an unverified client-declared write
  size in the first place.
- **Caching only non-empty writes (excluding `entry_size == 0` as the pre-amendment logic already
  did).** Rejected: the empty-write-poison path is independent of that guard — the exploit's
  poisoned value is the arbitrary `size_of_entry` an empty write (`count_of_entry: 0`) can still
  carry, which the pre-amendment "only exclude `entry_size == 0`" rule never excluded, since the
  poisoned example value (`0xFFFFFFFF`) is nonzero.
- **Bounds-checking `BorrowedVendorSpecificStructArray::bytes` against `ParamMaxEntries`.**
  Rejected: `ParamMaxEntries` is an entry *count*, not a byte *length* — multiplying it by the very
  `entry_size` under suspicion to get a byte bound is circular, it does not independently constrain
  anything `entry_size` itself could not already lie about.
- **An opt-in "trust client-declared sizes" config flag**, for a deployer who wants the old
  write-once-then-trust convenience back. Rejected: this is an unsafe-by-configuration knob that
  just reinstates the vulnerability behind a flag, directly violating this amendment's own
  fail-loudly principle (Decision item 4b/c) — a deployer who wants no config burden can already get
  it more safely by configuring `vendor_struct_types` once, which this ADR requires unconditionally
  regardless of that flag's existence.
- **Validating a write by immediately reading it back and comparing.** Rejected: circular — a
  read-back still needs a `size_of_entry` to interpret the returned bytes with, which is the exact
  fact this whole ADR exists to resolve; and moot in this codebase regardless, since
  `iso22900-mock`'s `PDUSetComParam` ignores its payload entirely (`tests/grpc_mock.rs`'s own doc
  comments), so a real round-trip can't even be exercised through the test double this service ships
  with.

## Consequences

**Implemented same-day as a direct follow-up to this ADR (2026-09-04); no design deviation from
the Decision section above.** Concretely:

- The three `unimplemented` arms in `convert.rs` are removed; standard/vendor STRUCTFIELD
  conversions (Decision items 1-3) are implemented in `convert.rs`'s
  `structfield_from_proto`/`structfield_to_proto` and their per-struct-type encode/decode helpers.
- `vci-service-config`'s `InstanceConfig` gained `vendor_struct_types` and a
  `find_vendor_struct_type_size` accessor (Decision item 4a).
- `docs/rpc-api-guide.md`'s "Communication Parameters" section documents the `type_url` grammar and
  the entry-size resolution order.

**Amended 2026-09-07 (Codex-review-found local heap-disclosure fix; no further design deviation
from the amended Decision section above).** Concretely:

- `Iso22900Service`'s process-lifetime `vendor_struct_cache` (originally Decision item 4b) and its
  `remember_vendor_struct_entry_size` method are deleted outright, along with every call site
  (`rpc_link.rs::rpc_set_com_param`, `rpc_misc.rs::rpc_set_unique_resp_id_table`) and the
  `convert::vendor_struct_write_of` helper that decided what counted as a cacheable write. The
  shared `resolve_vendor_struct_entry_size` method (`rpc.rs`) now resolves from
  `vendor_struct_types` config alone.
- `convert.rs`'s `to_iso_param`/`structfield_from_proto`/`vendor_struct_from_proto` (the
  `SetComParam` write path) and `from_unique_response_item` (the `SetUniqueRespIdTable` write path)
  now take the same `&mut dyn FnMut(E_PDU_CPST) -> Result<u32, Status>` resolver shape the read path
  already used (Decision item 5, extended); `rpc_link.rs`/`rpc_misc.rs` both pass
  `Iso22900Service::resolve_vendor_struct_entry_size` as that resolver.
- `iso22900/src/item/data/common.rs`'s `BorrowedVendorSpecificStructArray::bytes` and
  `BorrowedStructfieldData::struct_array_bytes_with_entry_size` are now `unsafe fn` (Decision item
  6); their call sites in `iso22900-service/src/service/convert.rs` and
  `iso22900/src/item/param.rs` are updated accordingly, and the standard-struct-type read call site
  in `convert.rs` switched to the already-safe `struct_array_bytes()` instead.
  `OwnedParamItem::from_borrowed_parts_impl`/`to_owned_with_vendor_entry_size_resolver`
  (`iso22900/src/item/unique_resp.rs`) gained a documented uncheckable-precondition doc comment
  instead of an `unsafe` signature.
- **Deliberate behavior change, not a bug:** an unconfigured vendor STRUCTFIELD write with
  `count_of_entry > 0` now fails `failed_precondition` where it previously silently succeeded and
  got cached for later reads. Every "or `SetComParam`/`SetUniqueRespIdTable` this struct type once
  first" remedy phrase is removed from error messages and docs — that path no longer exists. A
  deployer who previously relied on write-once-then-trust for a vendor struct type must now
  configure `vendor_struct_types` for it explicitly.
- **Accepted residual:** a misconfigured (wrong but present) `vendor_struct_types` entry is still
  undefined behavior on both the read and write paths — this ADR closes the *unverified,
  client-controlled* size source, not the general risk of a wrong-but-trusted config value. That
  residual sits in the same trust class as `library_path` itself: an operator who configures this
  service already controls what native code loads into this process, and a misconfigured entry
  size is a strictly smaller-blast-radius mistake than that. Not a new escalation this amendment
  introduces.
- Test coverage was added for: an unconfigured non-empty vendor struct write (`failed_precondition`,
  `service::rpc::tests`), a configured write with a matching `size_of_entry` (succeeds), a
  configured write with a mismatched `size_of_entry` (`invalid_argument` naming the configured
  size), an empty write with no config (still succeeds — the pre-existing behavior must not
  regress), the empty-write-poison shape itself (`size_of_entry: 0xFFFFFFFF, count_of_entry: 0`,
  succeeds but a later read of the same struct type still requires config), and the equivalent
  RPC-boundary versions of the unconfigured/mismatched cases in `tests/grpc_mock.rs`. Six
  pre-amendment cache-behavior tests in `service::rpc::tests` (covering a written-value cache hit,
  config outranking the cache, a misconfigured zero falling through to the cache, an empty write not
  clobbering a cached nonzero size, and `SetUniqueRespIdTable` populating the cache for one or
  several entries) tested behavior that no longer exists and were deleted, not kept as a historical
  record.
- `docs/rpc-api-guide.md`'s "Communication Parameters" section is rewritten: "config -> cache ->
  failed_precondition" becomes "config only; a non-empty write requires an exact match."
- `iso22900-service/docs/implementation-notes.md`'s Assumptions bullet describing the cache is
  rewritten to describe config-only resolution instead.
- `docs/worker-crates.md` and `docs/glossary.md`, which each described the cache, are updated to
  match.

**Amended 2026-09-07, PR #133 Codex review round (sibling of `j2534-0404-service`'s `vendor_ioctls`
key-grammar fail-fast fix, commit 063a9cb; no design deviation).** `find_vendor_struct_type_size`
constructs a canonical `"0x<8 lowercase hex digits>"` lookup key from a `struct_type`, but a
`config.toml` entry keyed with a noncanonical sibling (wrong case, wrong width) deserializes into
the string-keyed `vendor_struct_types` table without error and then never matches that lookup --
the entry is silently unreachable, failing every nonempty read/write of that struct type
`failed_precondition` as "unconfigured" even though an operator believes it is configured.
`vci-service-config` gained `validate_vendor_struct_type_keys`, a lightweight startup check
(mirroring `j2534-0404-service::config::parse_vendor_ioctl_entry`'s identical `vendor_ioctls`
grammar check) that scans both priority levels' `vendor_struct_types` tables for a configured
library and fails fast on the first noncanonical key found, naming it. `Iso22900Service::new` calls
it once at startup, alongside `library_path` resolution; `find_vendor_struct_type_size`'s existing
per-request lookup is unchanged. Unlike `j2534-0404-service`'s `resolve_vendor_ioctls` (which
eagerly loads its whole table into a `HashMap` field), `iso22900-service` still resolves
`vendor_struct_types` per request on demand -- this amendment adds a separate validation-only pass,
not an eager whole-table cache.

**Amended 2026-09-07, PR #133 Codex review round (rejecting the standard struct-type range from the
vendor size table; no design deviation).** A canonical key naming a standard `ComParamStructType`
(0x1-0x3, e.g. `"0x00000001"`) passed `validate_vendor_struct_type_keys`'s grammar check yet could
never actually be consulted: `read_borrowed_param_value` resolves 0x1-0x3 through
`standard_struct_entry_size`, and `vendor_struct_from_proto` rejects those values on the vendor
path outright. `validate_vendor_struct_type_table_keys` now also rejects a key in that range, so an
operator who mistakenly believes such an override is active fails fast at startup instead of
silently never having it take effect.

**Amended 2026-09-07, PR #133 Codex review round (two further `find_vendor_struct_type_size`/
`validate_vendor_struct_type_keys` gaps; no design deviation).** `find_vendor_struct_type_size`'s
arch-level lookup returned a configured `0` immediately, without falling through to the api-level
table; `resolve_vendor_struct_entry_size` treats a configured `0` as "not configured" (`size != 0`
filter), so a `0` placeholder at the arch level permanently shadowed a real nonzero api-level value,
failing every nonempty read/write precondition even though a usable config existed. The arch-level
branch now only returns early when its resolved size is nonzero. Separately,
`validate_vendor_struct_type_keys` used the lenient config loader (silently treating an unreadable
or malformed config file as "nothing configured"), on the stated assumption that a broken config
file independently fails startup elsewhere (e.g. resolving `library_path`) — but `find_library_path`
uses the same lenient loader, and `resolve_library_path` can fall back to platform auto-discovery
(RDF) when it sees `None`, so that assumption did not hold: a malformed document could pass this
validation silently while a real `vendor_struct_types` table went unvalidated. Switched to the
strict loader (`try_load_toml_config`, mirroring `find_modules`'s existing precedent), propagating a
read/parse error as a startup failure.

**Amended 2026-09-07, PR #133 Codex review rounds (write-side vendor struct buffer alignment; two
rounds, the second a design-advisor consult).** A vendor STRUCTFIELD write's `pStructArray` payload
was originally stored as `Vec<UNUM8>` (`Vec<u8>`, 1-byte-aligned by Rust's memory model), but the
native `PDUSetComParam`/`PDUSetUniqueRespIdTable` call casts that pointer to the vendor DLL's own
vendor-defined struct type per its own alignment requirements — a `Vec<u8>`-backed pointer
dereferenced as a wider type is undefined behavior. First fix: `StructFieldStorage::VendorSpecific`
backed by a new `AlignedVendorStructBuf` (`Vec<u64>`-backed, 8-byte alignment), mirroring
`j2534-0404-service::service::rpc_misc::AlignedByteBuf`'s identical fix for raw vendor IOCTL
buffers. A later Codex round correctly pointed out 8 bytes remains insufficient for a native vendor
struct type with fundamentally-16-byte alignment (e.g. `long double` on the x86_64 System V ABI).
Escalated to a design-advisor consult (rather than picking another fixed width ad hoc, the same
mistake being fixed): a vendor STRUCTFIELD's `pStructArray` is a `void *` into caller-allocated
memory, so by the C standard (C11/C17 §7.22.3, paraphrased) a conforming vendor DLL can only assume
*fundamental* alignment through it — the alignment an ordinary allocator guarantees, never an
extended (`_Alignas`/`__m256`-class) alignment, since no conforming C client could satisfy that
either. 16 bytes is at or above the fundamental alignment on every target this workspace's `*-sys`
crates build for (unlike `u128`, whose own alignment varies by target/toolchain — 8 bytes on 32-bit
ARM). `AlignedVendorStructBuf` is now backed by `Vec<Align16Chunk>`
(`#[repr(C, align(16))] struct Align16Chunk([u8; 16])`), a fixed, target-independent 16-byte-aligned
element, mirrored exactly in `j2534-0404-service::service::rpc_misc::AlignedByteBuf` (both copies
change together, ADR-136-style). Alignment was deliberately NOT made operator-configurable in
`vendor_struct_types` (unlike entry size): alignment requirements are monotone (a larger alignment
satisfies every smaller one), so a single fixed ceiling dominates every value in the *reachable*
domain (an untyped `void *` client buffer) with no residual there, whereas entry size has no
dominating value — that asymmetry is why size, not alignment, needed operator declaration.
**Accepted residual:** a vendor type genuinely requiring extended (>16-byte) alignment through this
`void *` boundary remains undefined behavior — unreachable by a conforming C client per the rule
above, so in the same trust class as the already-accepted "misconfigured-but-present size" residual.

**Amended 2026-09-08, PR #133 Codex review round (bound configured vendor struct entry sizes; no
design deviation, direct application of the sibling `vendor_ioctls` fix's already-established
pattern).** A configured `vendor_struct_types` entry size was accepted at any `u32` value with no
upper bound. `resolve_vendor_struct_entry_size` passes it straight to
`BorrowedVendorSpecificStructArray::bytes`, which multiplies it by the native entry count to build a
byte length for `ffi_slice`/`.to_vec()` — an operator typo (e.g. `0xffffffff`) reaches a native
result with even a single entry and attempts a multi-gigabyte allocation, aborting the service; a
smaller-but-still-oversized value reads beyond the DLL-owned allocation instead. This is the same bug
class `j2534-0404-service::config::VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES` (ADR-219, as amended) already
closed for the sibling `vendor_ioctls` raw-contract sizes, so no fresh design consult was needed here
— the ceiling-vs-cap distinction that fix's design-advisor consult established applies unchanged:
this is a **startup-rejection ceiling**, not an allocation cap substituted for the configured value.

`vci-service-config` gained `VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES = 64 KiB`, enforced by
`validate_vendor_struct_type_table_keys` (the same startup pass the key-grammar and standard-range
checks already run in) against every configured entry's value, at both priority levels. 64 KiB is
far smaller than `VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES`'s 16 MiB by design: every SAE/ISO-defined
ComParam struct this workspace encodes (`PDU_PARAM_STRUCT_SESS_TIMING`/`ACCESS_TIMING`/
`TLS_VERSION_AND_CIPHER`) is well under 64 bytes, and a vendor-specific STRUCTFIELD entry is a single
fixed-layout native record, not a bulk transfer like a raw vendor IOCTL buffer — so this ceiling has
no reason to approach that one's scale. `0` (the "not configured at this level" sentinel
`find_vendor_struct_type_size` already treats specially, per the 2026-09-07 arch-level-fallback
amendment above) is always within the ceiling and never rejected.

**Amended 2026-09-08, PR #133 Codex-review-loop pre-merge edge-case-hunter audit (correcting the
above amendment's residual framing; no design deviation).** The initial fix above enforced the
ceiling only in `validate_vendor_struct_type_table_keys` at startup, framed as "the same shape as
the key-grammar check's" residual. That framing was wrong in a way that mattered: the key-grammar
check's post-startup gap fails safe (a noncanonical key added after startup simply never matches
anything, so it stays inert), but this ceiling's post-startup gap did not — an operator (or
automation) editing `config.toml` to raise an already-validated entry's size above 64 KiB after
startup would reach `find_vendor_struct_type_size`'s per-request lookup (which re-reads the file
fresh on every call) with no re-check at all, flowing straight into
`BorrowedVendorSpecificStructArray::bytes`'s unchecked multiplication — the exact undefined-behavior
path this whole fix exists to close, not merely an OOM-abort. This also cut against this ADR's own
2026-09-07 amendment, which deliberately removed a process-lifetime cache specifically because
trusting anything not freshly re-validated was itself a heap-disclosure vector — a startup-only
ceiling for a table that is otherwise always re-read fresh was inconsistent with that already-decided
principle.

`find_vendor_struct_type_size` now re-checks the ceiling itself at every lookup (not just at
startup), filtering an oversized value the same way a `0` is already filtered at each priority level:
treated as "not configured at this level," falling through to the next level (arch → api), or to
`None` if there is none. This closes the gap without needing a `design-advisor` consult — extending
an already-correct filter (`size != 0`) already present in the same function to also cover
`size > VENDOR_STRUCT_TYPE_MAX_ENTRY_SIZE_BYTES` is the same kind of mechanical, precedented change
as the original fix, not a new design question. The startup check
(`validate_vendor_struct_type_table_keys`) is retained unchanged, for its fail-loud diagnostic value
(naming the offending key immediately rather than silently discarding it at request time) — the two
checks now serve different purposes (fail loud at startup, fail safe on every subsequent lookup)
rather than one being a weaker echo of the other. No accepted residual remains for this ceiling.

Test coverage: an entry size exactly at the ceiling (both priority levels, including the arch level
specifically) is still accepted, a size one above the ceiling is rejected naming the offending key at
startup, `u32::MAX` is rejected, `0` continues to be accepted (the existing "not configured" sentinel
is not disturbed), and an oversized arch-level entry falls through to a valid api-level value at
lookup time exactly as a `0` arch-level entry already did (mirroring the existing
`a_zero_arch_level_vendor_struct_type_size_falls_back_to_api_plus_lib` test).

Other consequences:

- The failure mode in Decision item 4b/c (an unconfigured vendor struct type's first read, or a
  non-empty write, fails loudly with `failed_precondition`) is by design, not a bug to be smoothed
  over later — it is the direct, intended consequence of there being no way to recover the entry
  size from the native API, and (as of the 2026-09-07 amendment) no way to trust one learned from an
  unverified client write either.
- Vendor-invented IOCTL item types remain impossible to express (Decision item 7) — an accepted
  residual of the D-PDU API's own closed `E_PDU_IT` enum, not a gap this ADR or a future one can
  close at the gRPC layer.
- ADR-178 is unaffected: this ADR introduces no new proto message, field, or RPC method.
