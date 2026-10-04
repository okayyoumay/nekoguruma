# ADR-219: `j2534-0404-service` Vendor IoctlID/ConfigParameterID Passthrough (0x10000+)

**Date:** 2026-09-04
**Status:** Accepted (implemented — see Consequences); amended 2026-09-07 (allowlist gate fix,
Decision item 3); amended again 2026-09-07 (Decision item 2's raw/wrapped-mode sizing replaced by a
per-`cmd_id` `vendor_ioctls` operator config contract — a third Codex review round found the fixed
64 KiB raw-mode cap itself unsafe and wrapped mode independently vulnerable to the same class);
amended further 2026-09-07 in additional PR #133 Codex review rounds the same day (noncanonical
`vendor_ioctls` key aliases rejected; below-vendor-range `cmd_id` entries rejected; raw-mode buffer
alignment fixed then widened from 8 to 16 bytes; required raw-mode buffer presence enforced
regardless of the other direction; the earlier "NULL/NULL on an unconfigured cmd_id needs no
config" exemption reversed, and `input_required`/`output_required` added to wrapped mode's own
contract, per a design-advisor consult; an unknown `vendor_ioctls` contract field now fails
startup instead of silently deserializing with a misspelled field's default; a `vendor_ioctls`
entry colliding with one of this service's own reserved `PDU_IOCTL_BASE` ids now fails startup
too; amended 2026-09-08 (a 16 MiB startup-rejection ceiling added to raw mode's `input_bytes`/
`output_bytes`, distinct from the removed allocation cap, per a design-advisor consult — see
Consequences)
**Affects:** `j2534-0404-service/src/service/rpc_misc.rs`, `j2534-0404-service/src/service/comparam_id.rs`,
`j2534-0404-service/src/service/comparam_support.rs`, `j2534-0404-service/src/service/service_params.rs`,
`j2534-0404-service/src/config.rs`, `j2534-0404-service/src/service.rs`, `vci-service-config/src/lib.rs`,
`j2534-0404/src/lib.rs`, `j2534-0404-mock`, `j2534-0404-service/docs/implementation-notes.md`,
`docs/rpc-api-guide.md`, `docs/j2534-0404-architecture.md`, `docs/worker-crates.md`,
`docs/glossary.md`, `j2534-0404-service/docs/comparam-protocol-support.md`, ADR-178, ADR-218

## Context

This ADR is the `j2534-0404-service` counterpart to ADR-218, part of the same broader effort to
expose vendor-specific native-DLL extensions through the existing shared gRPC proto, under the
same ADR-178 freeze (see ADR-218's Context for the freeze's exact scope and its one carve-out —
a genuinely new RPC method, which this ADR does not need either).

SAE J2534-1 (DEC2004) reserves the numeric range at and above `0x10000` for tool-manufacturer use,
for both `IoctlID` and `ConfigParameterID`:

- `IoctlID`: the Ioctl ID value table (§7.2.14.3,
  `vehicle-comm-specs/j2534-1-0404/J2534_1_200412 - Recommended Practice for Pass-Thru Vehicle
  Programming.md:1080-1099`) assigns `0x0F`-`0x7FFF` to SAE and `0x8000`-`0xFFFF` to SAE J2534-2,
  leaving `0x10000`-`0xFFFFFFFF` for the tool manufacturer (line 1099).
- `ConfigParameterID`: the GET_CONFIG/SET_CONFIG parameter-details table (§7.3.2, same file,
  lines 1224-1267) assigns the identical three-way split — `0x26`-`0x7FFF` SAE, `0x8000`-`0xFFFF`
  SAE J2534-2, `0x10000`-`0xFFFFFFFF` reserved for the tool/vehicle manufacturer (line 1267).

**IoctlID today.** `j2534-0404-service`'s `IoCtl` RPC dispatch (`rpc_misc.rs::rpc_io_ctl`,
starting line 916) has an explicit match arm per D-PDU-style adapter command — 25 arms total per
its own doc comment (lines 936-944) — and falls through, for any unmatched `cmd_id`, to
`rpc_io_ctl_legacy` (line 1166). `rpc_io_ctl_legacy` (defined at line 1248) itself recognizes only
the 4 legacy raw J2534 ids (`CLEAR_RX_BUFFER`/`CLEAR_TX_BUFFER`/`CLEAR_PERIODIC_MSGS`/
`CLEAR_MSG_FILTERS`, all `<= 0x14`, per its own doc comment lines 1239-1247) and returns
`Status::Unimplemented` for everything else via its catch-all arm (lines 1787-1801). Neither
`rpc_io_ctl_legacy`'s signature (`handle`, `cmd_id`, no `DataItem` in or out) nor its catch-all
carries any input/output payload today.

This service's own private, D-PDU-shaped internal IOCTL command ids live at `PDU_IOCTL_BASE =
0x2900_0000 + n` (`service_params.rs:761`) — a value inside the vendor range `0x10000`-`0xFFFFFFFF`.
Any vendor-passthrough dispatch arm added at or above `0x10000` therefore numerically overlaps this
service's own ~25 private ids. This is a naming/documentation concern, not a functional break: the
existing dispatch already matches all 25 of those ids in earlier, more specific arms before falling
through to the legacy/vendor catch-all (`rpc_io_ctl`'s match at lines 945-1169), so a request for one
of this service's own private ids is never misrouted to the new vendor-passthrough path. Called out
explicitly in Decision below as an accepted residual, not a functional collision.

**ConfigParameterID today.** There is no separate `SetConfig`/`GetConfig` gRPC method — J2534
config parameters are reached through the shared `SetComParam`/`GetComParam` RPC pair, the same
D-PDU ComParam ID space `iso22900-service` uses (ADR-027, "Type-Level Separation of ComParam IDs
from J2534 Native Config IDs"). `ComParamId::to_j2534_config_id(self, hw_protocol_id) ->
Option<u32>` (`comparam_id.rs:103-410`) translates a recognized D-PDU ComParam id to a native
J2534 `ConfigParameterID` via several protocol-specific match arms, falling back to a generic
match (lines 338-408) whose catch-all (`_ => false,`, line 407) makes every currently-unmapped id
return `None` — never forwarded to native `PassThruSetConfig`/`PassThruGetConfig`. A second call
site, `comparam_support.rs::strip_captured_channel_wide_keys` (lines 1747-1762, consulting
`to_j2534_config_id` at line 1755), also depends on this same translation.

Per SAE J2534-1, native `SCONFIG.Value` is always a plain `u32` — confirmed by the native wrapper,
`j2534-0404/src/lib.rs`'s `get_config`/`set_config` (lines 1263-1314), which operate on
`&[u32]`/`&[(u32, u32)]` throughout. No complex payload encoding is needed for a config value —
only an ID-space policy decision for which ids get forwarded.

**Critical collision fact — why the vendor boundary must be `0x10000`, not `0x8000`.** The
`0x8000`-`0xFFFF` (SAE J2534-2) range is already double-booked between native SAE J2534-2
`ConfigParameterID`s and this service's own minted D-PDU ComParam ids sharing the same numeric
window. A concrete, verified instance: `service_params.rs:232` mints
`PARAM_ACCESS_TIMING_ECU = ComParamId(0x804E)`, and `service_params.rs:234` mints
`PARAM_ACCESS_TIMING_OVERRIDE = ComParamId(0x804F)` — the exact same numeric values as the native
`CONFIG_TP2_0_IDENTIFER` (`0x804E`) and `CONFIG_TP2_0_RXIDPASSIVE` (`0x804F`) constants, as
`service_params.rs`'s own doc comments for the TP2.0 passive-connection ComParams note explicitly
(lines 557, 572-573: "despite the native `CONFIG_TP2_0_IDENTIFER` (`0x804E`) constant already
existing" / "corresponding native constant `CONFIG_TP2_0_RXIDPASSIVE`, `0x804F`"). These two pairs
do not collide functionally today only because `to_j2534_config_id` returns `None` for
`PARAM_ACCESS_TIMING_ECU`/`PARAM_ACCESS_TIMING_OVERRIDE` (they are never routed to native
`SET_CONFIG`/`GET_CONFIG` at all) — but it proves the `0x8000`-`0xFFFF` window is not free for a
generic vendor-identity passthrough to reuse: a same-valued ComParam id already means something
else entirely in this service's own numbering. `0x10000` is the only boundary consistent with both
J2534-1's own reservation table and this service's existing id allocations.

**Amendment (2026-09-07): a third Codex review round found Decision item 2's raw/wrapped-mode
sizing is unsafe in two ways, one of them newly discovered rather than a residual of the first two
fixes.** Two earlier rounds in this PR fixed undersized-buffer bugs (an out-of-bounds native write
when a small client-requested `output_capacity` was allocated literally, then the symmetric read-side
fix for undersized input) by always allocating a fixed `VENDOR_IOCTL_MAX_RAW_CAPACITY` (64 KiB)
backing buffer for raw mode regardless of client input. This third round found that fix was still
unsafe:

1. **The 64 KiB cap itself is an invented bound with no native-contract basis.** A real vendor
   command's true native I/O contract could legitimately need more than 64 KiB, and the native DLL
   writes past a smaller fixed allocation regardless, since raw mode communicates no capacity to the
   native side at all — the cap was chosen for this ADR's own convenience, not derived from any
   vendor command's real requirement.
2. **Wrapped mode is independently vulnerable, via the exact mechanism this ADR's own Decision item
   2 flags as a design choice: the CLIENT selects raw-vs-wrapped mode via flags bit 2.** A vendor
   command whose real native contract expects a raw buffer, if a client instead sets the wrapped
   flag bit, makes the native DLL write through `pOutput` pointing at `VendorIoCtl::Wrapped`'s small
   embedded `output_array: SBYTE_ARRAY` struct field (16 bytes) — an out-of-bounds write corrupting
   neighboring struct fields, in the very mode this PR's first two fixes assumed was already safe
   because its own allocation is sized to the client's own `output_capacity`. A client-controlled
   mode selector cannot be trusted to match a `cmd_id`'s real native contract; only the operator
   (who configured which vendor DLL is loaded) can state that mapping.

See Decision item 2 (as amended) and Consequences below for the fix, which mirrors ADR-218 Decision
item 4's identical precedent: operator config is the sole source of a native-only-known contract, no
client/default trust.

## Decision

**1. No new RPC method, no new proto fields — reuse `IoCtl` and `SetComParam`/`GetComParam`.**
Satisfies ADR-178: neither vendor space introduces a new handle or stream, so ADR-178's
Consequences carve-out for genuinely new RPC-level semantics does not apply here; this ADR stays
inside ADR-178's frozen default policy (a scalar-or-id/value-list-shaped capability reuses
ComParam staging, a heterogeneous/blob-shaped one reuses `DataItem.bytearray_data`).

**2. IoctlID: forward any `cmd_id` in `0x0001_0000..=0xFFFF_FFFF` raw to native `PassThruIoctl`.**
Add a new dispatch arm ahead of `rpc_io_ctl_legacy`'s catch-all (i.e., a sibling function invoked
from `rpc_io_ctl`'s own fallback, since `rpc_io_ctl_legacy` itself is `cll_handle`-only and carries
no payload today — see handle resolution below) that forwards via the existing generic extension
point in the native wrapper crate, `j2534_0404::J2534Api0404::ioctl<C: IoCtlCommand>` (unsafe,
`j2534-0404/src/lib.rs:1557`). Every unhandled id below `0x10000` continues to be rejected
`Unimplemented`, exactly as today: SAE-standard (`0x0F`-`0x7FFF`) and SAE J2534-2
(`0x8000`-`0xFFFF`) ids have SAE-defined structured pointer shapes and, where this service already
implements them, service-tracked state (filters, periodic-message slots, device config) that a raw
untyped bypass would desynchronize — only ids `>= 0x10000` are safe to hand off blind, since SAE
defines no structure or service-side state for them at all.

   - **Handle resolution.** `module_handle` maps to the native `DeviceID` (via the existing
     module-handle resolution `rpc_misc.rs::require_module_handle_for_ioctl`, line 901, already
     used by the 25-arm dispatch's module-scoped commands). `cll_handle` maps to the live channel
     id via the existing `resolve_live_legacy_link` helper (`rpc_misc.rs:1215`, requires a
     connected link) under the same `shared_channels`-outermost lock discipline ADR-080
     establishes and `rpc_io_ctl_legacy` already follows (`rpc_misc.rs:1255-1294`'s own comments).
     `system_handle` is rejected `invalid_argument`: a J2534 IOCTL targets a device (`DeviceID`) or
     a channel (`ChannelID`) natively — there is no native IOCTL target corresponding to this
     service's own top-level system handle, so there is nothing to translate `system_handle` into.
   - **Payload encoding.** Input via `DataItem.bytearray_data` with a documented hand-packed
     little-endian header (mirroring ADR-178's own Repeat Messaging/Device Configuration precedent
     of packing a structured payload into `IOBytearray`):
     - `u32 flags` — bit 0: input present; bit 1: output requested; bit 2: 0 = input/output are raw
       byte buffers, 1 = wrapped in a native J2534 `SBYTE_ARRAY` struct. This distinction exists
       because some native J2534 vendor IOCTLs expect a direct-value pointer (e.g. `u32 *`) while
       others expect an indirect `SBYTE_ARRAY`, and a remote gRPC client cannot construct a
       host-side pointer either way — the service must build whichever native shape the flag
       selects. Every other bit is reserved and must be `0` (`invalid_argument` otherwise), so a
       future mode can be added by a new flag bit without any ID-space change.
     - `u32 output_capacity` — `0` means no output pointer is passed (native `pOutput` is `NULL`);
       otherwise the service allocates a zeroed buffer before the native call, sized per the
       `vendor_ioctls` config rule below. `invalid_argument` if the output-requested flag bit is set
       but `output_capacity == 0`.
     - Remaining bytes: raw input bytes — empty means the input pointer is `NULL` (raw mode) or an
       empty `SBYTE_ARRAY` (wrapped mode).
     - **Output** is returned via `DataItem.bytearray_data`: in raw mode, the entire
       `output_capacity`-sized buffer is returned as-is, since raw mode has no length-reporting
       convention and the service cannot know how many bytes the native call actually wrote; in
       wrapped mode, `min(NumOfBytes, output_capacity)` bytes are returned, since the `SBYTE_ARRAY`
       self-reports its own length.
   - **Rejected alternative:** mapping `unum32_value`/`bytearray_data` oneof variants directly onto
     raw-pointer vs. `SBYTE_ARRAY` shapes with no header at all. Rejected because the output shape
     and capacity still need to be communicated somehow — this would create two encodings for one
     mechanism (an implicit oneof-variant-selects-mode convention, plus a still-needed
     capacity/flags side-channel) instead of one explicit, flags-driven encoding.
   - **Operator config is the sole source of a vendor IOCTL's native contract (as amended
     2026-09-07, mirroring ADR-218 Decision item 4's identical governing precedent — operator config
     as the sole source of a native-only-known contract, no client/default trust).** A per-library
     `[config.apis.j2534-0404.libs."<lib>".vendor_ioctls."0x<8 lowercase hex digits>"]` table (the
     key is `cmd_id`, same grammar ADR-218's `vendor_struct_types` uses) declares each vendor IOCTL
     command's full native contract, required for any command that carries a non-NULL buffer in
     either direction:
     - `shape`: `"raw"` or `"sbyte_array"` — MUST match the request's flags bit 2 selection exactly,
       or the call is rejected `invalid_argument` naming the mismatch. The client still picks which
       wire shape it sends (flags bit 2 is unchanged, no wire-format change), but the configured
       shape is the only authority on whether that selection is safe for this `cmd_id` — a client
       guessing wrong is rejected, never honored.
     - `input_bytes`/`output_bytes` (`u32`, `shape = "raw"` only): the exact number of bytes the
       vendor DLL reads from `pInput`/writes to `pOutput`; `0` means that direction takes no buffer.
       A nonzero value also mandates that direction's presence (see dispatch-time rules below). Not
       meaningful for `shape = "sbyte_array"` — wrapped mode's own `SBYTE_ARRAY.NumOfBytes`
       mechanism already self-describes the real written length safely, up to whatever the service
       allocates for it (see the retained 64 KiB constant below); setting these fields on a
       `"sbyte_array"` entry is a startup error (as amended 2026-09-07, seventh round — see
       Consequences), not a silent no-op.
     - `input_required`/`output_required` (`bool`, `shape = "sbyte_array"` only, default `false`,
       as amended 2026-09-07, seventh round — see Consequences): whether this command's native
       contract always expects a non-NULL `SBYTE_ARRAY` in that direction — the wrapped-mode
       counterpart to `input_bytes`/`output_bytes` implying the identical requirement, since
       wrapped mode has no byte count of its own to double as one. Setting these fields on a
       `"raw"` entry is a startup error, for the same reason as above.
     - Loaded once at startup into `J2534Service::vendor_ioctls` and fail-fast validated (an
       unrecognized `shape` string or malformed hex key fails startup), mirroring the existing
       `resolve_modules`/`CanChannelMode::from_config` precedent — never re-read from `config.toml`
       per dispatch.
     - **Dispatch-time rules**, evaluated in `rpc_io_ctl_vendor` BEFORE any handle resolution or
       `shared_channels`/`api` lock is taken, so a rejected call never touches those locks:
       - **Every vendor `cmd_id` must have a configured entry, with no exception for a nominally
         bufferless request** (as amended 2026-09-07, seventh round — see Consequences; this
         reverses this Decision section's own earlier text, which is why the two bullets below no
         longer read as they originally did). A request against a `cmd_id` with no configured entry
         is rejected `failed_precondition`, naming the `cmd_id` and the remedy (configure
         `vendor_ioctls` for this library), regardless of what buffers the request carries — even
         one carrying no input and requesting no output. A genuinely bufferless vendor command is
         allowlisted as `shape = "raw"` with both `input_bytes`/`output_bytes` left at `0` (the
         default) — that contract already means exactly "NULL/NULL only" per the rule below.
       - In raw mode with a config entry: the backing input/output allocations are sized EXACTLY to
         the configured `input_bytes`/`output_bytes` (never a fixed cap) — client input shorter than
         `input_bytes` is zero-padded up (the same `pad_to_capacity`-style behavior the earlier two
         fixes used, just parameterized by the configured size instead of a fixed constant); client
         input LONGER than `input_bytes`, or a requested `output_capacity` GREATER than the
         configured `output_bytes`, is rejected `invalid_argument`. A nonzero `input_bytes`/
         `output_bytes` also mandates that direction's presence (a present, non-empty input / a
         nonzero `output_capacity`) regardless of what the other direction carries — an omitted or
         explicitly-empty input against a nonzero `input_bytes`, or no output requested against a
         nonzero `output_bytes`, is rejected `invalid_argument` (as amended 2026-09-07, sixth
         round — see Consequences).
       - In wrapped mode with a config entry: `input_required`/`output_required` mandate that
         direction's presence as outer-`SBYTE_ARRAY*` non-NULL-ness (`request.input.is_some()` for
         input — a present-but-empty wrapped input still becomes a real non-NULL `SBYTE_ARRAY` with
         `NumOfBytes: 0`, satisfying this; `output_capacity > 0` for output), rejected
         `invalid_argument` otherwise (as amended 2026-09-07, seventh round — see Consequences).
   - **The fixed raw-mode allocation cap is removed; the constant is repurposed as a WRAPPED-MODE-ONLY
     resource sanity bound.** `VENDOR_IOCTL_MAX_RAW_CAPACITY` is renamed
     `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY` (still 64 KiB) and no longer applies to raw mode at all —
     raw mode's bound now comes entirely from the configured `output_bytes`/`input_bytes`, which may
     legitimately exceed 64 KiB. It remains a sanity cap on wrapped mode's own output allocation
     only, since wrapped mode's `SBYTE_ARRAY.NumOfBytes` already self-describes the real written
     length regardless of how large the backing allocation is — this is a resource-consumption bound
     on a client-driven allocation, not a safety-critical one the way the raw-mode cap was mistakenly
     treated as. **As amended (Codex review, PR #133 eleventh round; design-advisor decision — see
     Consequences): removing the raw-mode allocation CAP does not mean raw mode's configured
     `input_bytes`/`output_bytes` are unbounded.** A startup-rejection CEILING
     (`VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES`, 16 MiB) is not the same mechanism as the removed
     allocation cap: the cap substituted a SMALLER allocation than what was configured, letting a
     native call proceed against an undersized buffer (silent corruption) — the ceiling never
     substitutes anything; it refuses to start at all for a configured value it considers infeasible,
     the identical fail-fast treatment this table already gives a malformed shape or a colliding
     `cmd_id`.
   - **Accepted residual:** `SCONFIG_LIST`-shaped vendor IOCTLs (a third native shape beyond
     raw-pointer and `SBYTE_ARRAY`) are not covered by this two-mode header. It can be added later
     as a third flag-bit mode with no further proto or ID-space change needed; if added, it would
     become a third `shape` value in the `vendor_ioctls` config contract too.
   - This service's own ~28 `PDU_IOCTL_BASE` (`0x2900_0000+`) private ids fall within the same
     `0x10000`-`0xFFFFFFFF` vendor range this decision opens for passthrough, and the existing
     dispatch already matches all of them in more specific arms first, ahead of the vendor
     fallback. **As amended (Codex review, PR #133 tenth round; see Consequences):** an operator's
     own `vendor_ioctls` config landing on one of these already-reserved ids (no relocation of the
     constants required) is now rejected at startup rather than left as an accepted residual — see
     the amendment for why the original framing (a future-maintainer-relocation risk only) was
     incomplete.
   - **Accepted residual (unchanged in kind by this amendment):** a wrong-but-present
     `vendor_ioctls` entry (e.g. a `shape`/size that does not actually match the connected library's
     real native contract) is still undefined behavior — this amendment closes the *unverified,
     client-controlled* contract source, not the general risk of a wrong-but-trusted config value.
     That residual sits in the same trust class as `library_path`/ADR-218's identical residual: an
     operator who configures this service already controls what native code loads into this process,
     and a misconfigured contract is a strictly smaller-blast-radius mistake than that.

**3. ConfigParameterID: identity passthrough.** Extend `to_j2534_config_id`'s generic-match
catch-all (`comparam_id.rs:407`, currently `_ => false,`) so that `self.0 >= 0x10000` also yields
`true`, making the function's existing `supported.then_some(self.0)` tail (line 409) return
`Some(self.0)` — an unchanged-value (identity) passthrough of the same numeric id, exactly the
same shape `to_j2534_config_id` already uses for its other directly-forwarded ids (e.g.
`DATA_RATE`/`LOOPBACK`, matched identically at line 339, returned via the same `self.0` tail — not
a new pattern this ADR invents). No namespacing or offset scheme is introduced. The `0x10000`
boundary (not `0x8000`) is mandatory, for the load-bearing reason stated in Context: the
`0x8000`-`0xFFFF` window is already double-booked by this service's own minted ComParam ids
(`PARAM_ACCESS_TIMING_ECU`/`PARAM_ACCESS_TIMING_OVERRIDE` at `0x804E`/`0x804F`, confirmed
colliding with native `CONFIG_TP2_0_IDENTIFER`/`CONFIG_TP2_0_RXIDPASSIVE`) — reusing that window
for a second, generic meaning would create real collisions, not merely a naming overlap.

   - **Value oneof:** `Unum32` only — native `SCONFIG.Value` is always `u32` (Context, above).
   - **Class/reporting:** report a vendor `>= 0x10000` config id as `PDU_PC_SPECIFIED`
     ("unclassified/not reported") — the same "real classes only where behaviorally justified,
     silent `PDU_PC_SPECIFIED` otherwise" policy ADR-133 (`j2534-0404-service`'s own
     `GetComParam`-class-reporting ADR) already establishes for every id this service cannot
     otherwise classify. Leave the `BUSTYPE`/`TESTER_PRESENT` special-handling lists
     (`comparam_support.rs`) untouched — this service cannot know a vendor id's semantic class, so
     it must not special-case it into either list. A vendor id does still get pushed through
     whatever generic temp-param-update/staging bracket mechanism applies uniformly regardless of
     class (that mechanism is not class-specific, so nothing needs to change there for it to keep
     working for a vendor id).
   - **Read behavior for an unstaged vendor id** (`GetComParam` called before any `SetComParam` for
     that id on this link): if a `Working`-staged entry exists, report it; else, if the link is
     connected, perform a live `PassThruGetConfig` read (`get_config_u32`,
     `j2534-0404/lib.rs:1287`) and report that value **without** inserting it into the `Working`
     staging table (avoids silently mutating staged state from a mere read); else (`Working` empty
     **and** not connected), return `failed_precondition`. Reporting a fabricated `Unum32(0)`
     default is explicitly rejected in every one of these branches — this service has no legitimate
     default for an id it does not recognize, the same "no fabricated not-configured-but-looks-like-
     a-value" principle ADR-133 already establishes for this service's unseeded Bytefield/Structfield
     `GetComParam` fallback.
   - **Second call site.** `comparam_support.rs`'s `strip_captured_channel_wide_keys`
     (lines 1747-1762) also calls `to_j2534_config_id` (line 1755), over
     `CHANNEL_WIDE_UNUM32`'s fixed id list — a vendor id is never a member of that fixed list, so
     this call site is structurally unaffected by this Decision and needs no change; noted here so
     the second call site is not mistaken for an overlooked follow-up.
   - **Allowlist gate (design-advisor decision, same-day amendment).**
     `comparam_support.rs::is_param_allowed` admits any `ComParamId` satisfying the new
     `ComParamId::is_vendor()` predicate (`>= 0x10000`, `comparam_id.rs`) unconditionally, as its
     first check, ahead of every protocol-specific branch below it — including the closed-list ones
     (Honda DIAG-H, Analog Inputs, Ethernet_NDIS, UART Echo Byte) that otherwise reject even
     `DATA_RATE`/`LOOPBACK`. Without this, `is_param_allowed`'s per-protocol allowlist rejected a
     vendor id with `PDU_ERR_COMPARAM_NOT_SUPPORTED` before `to_j2534_config_id` was ever consulted,
     on any protocol this service itself recognizes — only reachable via the allowlist's
     pre-existing "Unknown protocol — allow" fallback (a genuinely unrecognized protocol id). ADR-028
     defines this allowlist as answering "may a D-PDU client reference this ComParam on this
     protocol," a question derived from SAE/ISO parameter tables (J2534-1 §7.3.2 and the D-PDU
     ComParam tables ADR-028 itself cites) — but SAE J2534-1's tool-manufacturer reservation
     (`0x10000`-`0xFFFFFFFF`, Context above) carries no per-protocol qualification at all, so no
     SAE-derived table can answer that question for a vendor id one way or the other. The vendor
     DLL's own `SET_CONFIG`/`GET_CONFIG` result is therefore the only authority on whether a given
     vendor id is meaningful for a given protocol, not this allowlist. The existing closed lists'
     silent-no-op exclusions (`PARAM_ANALOG_SAMPLE_RATE`/the `PARAM_TP20_*` block/`PARAM_NDIS_PIN_OPTION`/
     the `PARAM_UEB_*` block, each excluded from the "Unknown protocol" fallback because staging them
     on an unrelated protocol has no effect while `GetComParam` would keep reporting them as set) do
     not apply to a vendor id: `to_j2534_config_id`'s identity catch-all always translates and
     forwards a vendor id to native `SET_CONFIG`/`GET_CONFIG` (Decision item 3 above), so a vendor id
     staged on any protocol is never a silent no-op the way those service-minted ids are on an
     unrelated protocol. The boundary check is expressed once, as `ComParamId::is_vendor()` in
     `comparam_id.rs`, and shared by this gate, `to_j2534_config_id`'s identity catch-all, and
     `rpc_link.rs`'s unstaged-vendor-read gate, instead of three independent `>= 0x10000` literals.
     **Invariant this depends on:** no `ComParamId` `service_params.rs` mints may ever be `>= 0x10000`
     — every `PARAM_*` constant there currently sits in `0x8001`-`0x80E3`, well below the boundary
     (enforced by a dedicated unit test enumerating them). If that invariant were ever violated, this
     early return would silently shadow that id's own protocol-specific exclusion, admitting it on
     every protocol regardless of the allowlist it would otherwise be subject to.

### Alternatives Considered

- **`google.protobuf.Any`.** Rejected: same rationale as ADR-218 — no self-description problem
  exists to justify it, and it would import well-known-types machinery into a proto file ADR-178
  freezes against exactly this kind of unnecessary growth.
- **A new dedicated RPC method for vendor IOCTL/config access.** Rejected: neither vendor space
  introduces a new handle or stream, so ADR-178's narrow carve-out for genuinely new RPC-level
  semantics does not apply — reusing `IoCtl`/`SetComParam`/`GetComParam` is both sufficient and
  consistent with ADR-178's default policy.
- **Forwarding every unrecognized id, including below `0x10000`.** Rejected: this would bypass
  SAE-defined structured payload shapes and this service's own tracked state (filters, periodic
  slots, device config) for ids SAE has already defined a real meaning for — exactly the
  desynchronization risk Decision item 2 identifies.
- **Reporting `Unum32(0)` for an unstaged, unconnected vendor config read.** Rejected: a fabricated
  default the service has no basis for — the same defect class ADR-133 already closed for this
  service's Bytefield/Structfield fallback.
- **Per-protocol-helper insertion for the allowlist gate fix.** Rejected: adding the vendor
  admission check separately inside each of the 10+ per-protocol branches (and each closed inline
  `matches!` list) `is_param_allowed` already has, instead of one early return ahead of all of them.
  Drift-prone — a future new protocol branch could easily omit its own copy, silently re-closing the
  gap for that one protocol.
- **Get-only admission (accept a vendor id for `GetComParam` but not `SetComParam`, or vice versa).**
  Rejected: no SAE/ISO clause distinguishes get-reachability from set-reachability for a
  tool-manufacturer-reserved id, and both RPCs already share one `check_param_allowed` call site —
  splitting them would need a second, parallel gate function for no spec-grounded reason, and would
  break the ordinary `SetComParam`-then-`GetComParam` round trip Decision item 3 otherwise gives a
  vendor id.
- **Forwarding vendor ComParamIds in a separate `SET_CONFIG` batch from standard ComParams, so a
  vendor id's native rejection can't fail the standard-ComParam batch too.** Rejected for now as an
  unneeded extra mechanism — `apply_j2534_params`'s existing single-batch-per-connect shape already
  matches native whole-batch `SET_CONFIG` semantics (see Consequences below), and no concrete
  failure mode motivating the split has been observed. Noted here as possible later hardening if a
  real vendor DLL's rejection behavior ever makes the coupling a practical problem.

**Added 2026-09-07, evaluated when closing the raw-mode-cap/wrapped-mode-shape-mismatch finding
(design-advisor decision):**

- **A smaller fixed floor for an unconfigured raw command, instead of requiring `vendor_ioctls`
  config.** Rejected: still an invented constant with no basis in any specific command's real native
  contract, and still corrupts memory for a command whose real need exceeds the smaller floor — this
  is the same defect class as the original 64 KiB cap, just at a different magnitude.
- **Dropping raw mode entirely, forwarding every vendor IOCTL wrapped-only.** Rejected on two
  independent grounds: it loses direct-value `u32 *`-shaped vendor commands the real J2534 API
  supports (Decision item 2's own "e.g. `u32 *`" example), AND wrapped mode alone is not
  client-safe either, per this amendment's own finding — a raw-contract command driven through the
  small embedded `SBYTE_ARRAY` struct field is exactly the vulnerability being closed, so
  wrapped-only would not have fixed anything.
- **A new self-describing wire-format mode** (e.g. the client declaring its own claimed buffer size
  self-certified as correct). Rejected: it still needs an operator-configured contract to pick a
  safe shape/size in the first place — a client's own self-declared size is exactly the untrusted
  input this amendment closes, so this would be a wire change with no safety gain.
- **Configured sizes as a cap ON TOP OF a retained 64 KiB default for unconfigured commands**
  (rather than requiring config unconditionally for any buffer-carrying command). Rejected: the
  default IS the vulnerability — any fallback that lets a buffer-carrying vendor IOCTL proceed
  without an operator-declared contract reopens the same unverified-native-contract trust this
  amendment exists to close.

## Consequences

**This ADR originally authorized design only, with implementation deferred; implementation has
since landed (same-day follow-up), matching this Decision section exactly** — the vendor-IOCTL
dispatch arm (Decision item 2), the `to_j2534_config_id` identity catch-all and unstaged-read
`GetComParam` behavior (Decision item 3), `j2534-0404-mock` support for both surfaces, and the
test coverage and doc updates listed below are all in place. A residual surfaced during that first
implementation pass — `comparam_support.rs::is_param_allowed`'s per-protocol allowlist gated
`SetComParam`/`GetComParam` *before* `to_j2534_config_id` was ever consulted, making a vendor
`ConfigParameterID` reachable only on a CLL whose protocol this service does not itself recognize,
not on a recognized protocol like CAN — is now resolved by the same-day design-advisor-decided
allowlist-gate amendment recorded under Decision item 3 above, not left as an open backlog item.

One behavioral consequence of the fix: a rejected vendor id now fails the connect-time `SET_CONFIG`
batch (`apply_j2534_params`) as a whole, rather than failing individually at `SetComParam` time —
`apply_j2534_params` already applies every translated ComParam (standard and, as of this amendment,
vendor) in one native `PassThruIoctl SET_CONFIG` call per connect, so a vendor id the connecting
vendor DLL itself rejects fails that entire batch. This matches native J2534 `SET_CONFIG` semantics
(a single call carrying multiple parameters, one pass/fail outcome) rather than inventing a
partial-batch-success behavior this service does not otherwise have; `j2534-0404-mock` mirrors this
whole-batch behavior already (it has no independent per-parameter validation path either).

Other consequences:

- **`j2534-0500-service` is explicitly out of scope.** It remains an unimplemented-RPC stub per
  ADR-031 — named here explicitly so it is not accidentally assumed covered by this ADR.
- **Deferred, not designed here:** vendor extensions to `ProtocolID`, `FilterType`, and the
  currently-unused high bits of `TxFlags`/`RxStatus` are named as explicit future work a later ADR
  would need to address; this ADR covers only IoctlID and ConfigParameterID.
- `docs/rpc-api-guide.md` (the IOCTL byte-layout header table), `docs/j2534-0404-architecture.md`,
  and `j2534-0404-service/docs/comparam-protocol-support.md` (whose "Workaround: Use
  vendor-specific J2534 IOCTL commands..." note now points at the implemented passthrough, not
  just this ADR's design) all carry the final implementation details as of the same-day
  implementation follow-up noted above.
- ADR-178 is unaffected: this ADR introduces no new proto message, field, or RPC method.

**Amended 2026-09-07 (Codex-review-found raw-mode-cap/wrapped-mode-shape-mismatch fix, design-advisor
decision; no further design deviation from the amended Decision section above).** Concretely:

- `vci-service-config`'s `InstanceConfig` gained `vendor_ioctls: Option<HashMap<String,
  VendorIoctlConfigEntry>>` and a new `find_vendor_ioctls` accessor (whole-table, same two-level
  arch-over-api-lib precedence as `find_library_path`/`find_vendor_struct_type_size`) —
  `VendorIoctlConfigEntry { shape: String, input_bytes: u32, output_bytes: u32 }`.
- `j2534-0404-service/src/config.rs` gained `VendorIoctlContract` (`Raw { input_bytes, output_bytes }`
  / `SbyteArray`), `parse_vendor_ioctl_entry`, and `resolve_vendor_ioctls`, loaded once at startup
  into a new `J2534Service::vendor_ioctls: Arc<HashMap<u32, VendorIoctlContract>>` field
  (`service.rs::new`, fail-fast alongside `modules`/`can_channel_mode`).
- `rpc_misc.rs` gained `validate_vendor_ioctl_buffers`, called by `rpc_io_ctl_vendor` immediately
  after decoding the request and before any handle resolution or lock acquisition. `VendorIoCtl::new`
  now takes `raw_input_bytes`/`raw_output_bytes` (the resolved contract's configured sizes, `0`/`0`
  when wrapped or bufferless) instead of relying on a fixed constant; `pad_to_max_capacity` is
  renamed `pad_to_capacity` and takes an explicit target size. `VENDOR_IOCTL_MAX_RAW_CAPACITY` is
  renamed `VENDOR_IOCTL_MAX_WRAPPED_CAPACITY` and its enforcement moved from
  `unpack_vendor_ioctl_request` (mode-agnostic) to dispatch time (wrapped-mode-only; raw mode's bound
  now comes from the configured contract instead).
- **Deliberate behavior change, not a bug:** every vendor IOCTL that carries a non-NULL input or
  requested output now needs an operator-configured `vendor_ioctls` entry — previously any `cmd_id`
  worked (raw mode silently padded to the fixed 64 KiB cap; wrapped mode was unbounded by
  contract entirely). A deployer who previously relied on the fixed-cap behavior for a buffer-carrying
  vendor IOCTL must now configure `vendor_ioctls` for it explicitly.
- Test coverage was added for: an unconfigured buffer-carrying `cmd_id` in both raw and wrapped mode
  (`failed_precondition`), a configured-raw command receiving the wrapped flag bit and vice versa
  (`invalid_argument` each way), an `output_capacity`/input length exceeding the configured
  `output_bytes`/`input_bytes` (`invalid_argument` each), a non-empty input against a configured
  `input_bytes == 0` (`invalid_argument`), a NULL/NULL request on a completely unconfigured `cmd_id`
  still forwarding successfully, the pre-existing three raw-mode regression tests continuing to pass
  once the mock library's `vendor_ioctls` config is supplied by the test harness, and the
  fixed-64-KiB-cap regression test moved to the wrapped-mode path (where that cap still legitimately
  applies) — all in `j2534-0404-service/tests/grpc_mock/vendor_passthrough.rs`, plus new unit tests
  in `rpc_misc.rs` (`validate_vendor_ioctl_buffers_tests`), `config.rs`, and `vci-service-config`.
- `docs/rpc-api-guide.md`'s "IO Control" section, `docs/j2534-0404-architecture.md`'s "Vendor
  IoctlID/ConfigParameterID passthrough" note, `docs/worker-crates.md`'s `vci-service-config`
  entry, `docs/glossary.md`'s "IOCTL" entry, and
  `j2534-0404-service/docs/implementation-notes.md`'s Assumptions section are all updated to describe
  the `vendor_ioctls` config table and per-command shape/size contract requirement in place of the
  fixed-cap design.
- **Accepted residual (unchanged in kind):** the `SCONFIG_LIST`-shaped vendor IOCTL residual noted
  under Decision item 2 above is unaffected by this amendment in substance — it would still need a
  third `shape` value in `vendor_ioctls` if ever added, not a different mechanism.
- ADR-178 is unaffected: this amendment introduces no new proto message, field, or RPC method — the
  wire-level `flags`/`output_capacity` header is unchanged; this is purely a service-side
  allocation-sizing and rejection-policy change gated by new operator config.

**Amended 2026-09-07, PR #133 Codex review round (noncanonical `vendor_ioctls` keys silently
collapsing; no design deviation).** A `config.toml` key such as `"0x10001"` or `"0x00010001"` in
uppercase both decode to the same `cmd_id`, but `resolve_vendor_ioctls` collects the whole table
into one `HashMap<u32, VendorIoctlContract>`, silently retaining whichever alias's contract is
encountered last in an iteration order that is not stable across process starts — a misconfigured
alias could select a different trusted native contract on different runs. `parse_vendor_ioctl_entry`
now enforces the canonical `"0x<8 lowercase hex digits>"` grammar at parse time (same rule
`vendor_struct_types` already enforces via `validate_vendor_struct_type_keys`), rejecting a
noncanonical alias outright rather than letting it collapse.

**Amended 2026-09-07, PR #133 Codex review round (below-vendor-range `vendor_ioctls` entries;
no design deviation).** A canonical key such as `"0x00000001"` parsed successfully and startup
succeeded, but `rpc_io_ctl` only consults `vendor_ioctls` for `cmd_id >= 0x10000` — a lower `cmd_id`
always takes the legacy (non-vendor) path, so its configured shape/sizes were silently never
consulted. `parse_vendor_ioctl_entry` now rejects any `cmd_id` below `0x0001_0000` at startup,
matching the exact boundary `rpc_io_ctl` consults this table for.

**Amended 2026-09-07, PR #133 Codex review round (raw-mode buffer alignment; two rounds, the second
a design-advisor consult).** Raw mode's `input`/`output` buffers were originally a plain `Vec<u8>`
(1-byte alignment by Rust's memory model), but a raw vendor IOCTL's native contract can dereference
`pInput`/`pOutput` as any type it chooses (e.g. `u32 *`, `MOCK_VENDOR_IOCTL_RAW_U32`'s own
documented shape) — an insufficiently aligned pointer is undefined behavior regardless of what any
particular allocator happens to do in practice. First fix: `VendorIoCtl::Raw`'s `input`/`output`
backed by a new `AlignedByteBuf` (`Vec<u64>`-backed, 8-byte alignment). A later Codex round correctly
pointed out 8 bytes remains insufficient for a native type with fundamentally-16-byte alignment
(e.g. `long double` on the x86_64 System V ABI). Escalated to a design-advisor consult, decided
jointly with the identical question for `iso22900`'s sibling `AlignedVendorStructBuf` mechanism (see
ADR-218's equivalent amendment for the full rationale): `PassThruIoctl`'s `pInput`/`pOutput` are
`void *` into caller-allocated memory, so by the C standard (C11/C17 §7.22.3, paraphrased) a
conforming vendor DLL can only assume *fundamental* alignment through them, and 16 bytes is at or
above the fundamental alignment on every target this workspace's `*-sys` crates build for (unlike
`u128`, whose own alignment varies by target/toolchain). `AlignedByteBuf` is now backed by
`Vec<Align16Chunk>` (`#[repr(C, align(16))] struct Align16Chunk([u8; 16])`), mirrored exactly in
`iso22900::encode::AlignedVendorStructBuf` (both copies change together, ADR-136-style). Alignment
was deliberately NOT added to `vendor_ioctls`' `Raw` shape as an operator-configurable field (unlike
`input_bytes`/`output_bytes`): alignment requirements are monotone, so one fixed ceiling dominates
every value in the reachable (`void *`) domain, unlike size, which has no dominating value.
Wrapped mode's `SBYTE_ARRAY`-based buffers are unaffected — the native side already treats those as
a byte-oriented array per the J2534 spec's own `SBYTE_ARRAY` convention, not a wider-typed direct
pointer. **Accepted residual:** a vendor command genuinely requiring extended (>16-byte) alignment
through `pInput`/`pOutput` remains undefined behavior — unreachable by a conforming C client per the
rule above, in the same trust class as ADR-218's already-accepted "misconfigured-but-present size"
residual.

**Amended 2026-09-07, PR #133 Codex review round (required raw-mode buffers bypassable; no design
deviation).** `validate_vendor_ioctl_buffers`'s top-level "no buffers present" short-circuit ran
before any contract lookup, so a client could omit an input (or send an explicitly empty one) and
leave `output_capacity` at `0` against a `cmd_id` whose configured `Raw` contract declares a nonzero
`input_bytes`/`output_bytes` — bypassing the contract and forwarding a `NULL` pointer to a native
call that always reads/writes through it, crashing the service. The presence exemption now applies
only when `contract` is `None` (an unconfigured `cmd_id` still needs no buffers); within the `Raw`
branch, a nonzero `input_bytes`/`output_bytes` now unconditionally requires a present, non-empty
input / a nonzero `output_capacity` respectively, regardless of what the other direction carries.
Wrapped-mode (`SbyteArray`) contracts are unaffected, since they carry no per-direction byte counts
to enforce a "this direction is mandatory" requirement against.

**Amended 2026-09-07, PR #133 Codex review round (reversing the "NULL/NULL on an unconfigured
`cmd_id` needs no config" exemption; wrapped-mode `input_required`/`output_required` added; a
design-advisor consult).** The immediately preceding amendment's own closing sentence ("Wrapped-mode
contracts are unaffected... a buffer-less request against a configured `sbyte_array` `cmd_id` is
unaffected by this fix") turned out to identify only half of a wider gap Codex found in the very
next review round: (1) a configured `sbyte_array` command with no way to express a mandatory
direction could still be dispatched with a NULL outer `SBYTE_ARRAY*` even though its native contract
always expects one; and, more fundamentally, (2) the `contract.is_none()` branch's "a bufferless
request needs no config" exemption (this Decision section's own text, and ADR-218 Decision item 4d's
analogy it was modeled on) was itself unsound: a vendor `cmd_id`'s `pInput`/`pOutput` requirements
are a property of the `cmd_id` alone (SAE J2534-1 SS7.2.14, paraphrased), not of what a particular
CLIENT request happens to ask for, so an unconfigured command whose native side unconditionally
dereferences one of those pointers can still crash even when the client chose to send neither — the
same "trust the client, not the operator config" gap this Decision section's Operator-config-is-the-
sole-source rule already forbids for the buffer-carrying case, just not recognized as applying to the
buffer-less case too. The ADR-218 4d analogy does not actually transfer: that precedent concerns a
fully-typed standard D-PDU call whose semantics this service already knows, whereas a vendor `cmd_id`
is opaque to this service in its entirety.

Escalated to a design-advisor consult (this reverses an established Decision, not a straightforward
bug fix). Decision, applied to both gaps together since they share one root cause (the contract
schema's inability to express "this direction/command is mandatory" for anything but `Raw`'s two
byte-count fields):

- **The `contract.is_none()` exemption is deleted outright, not narrowed.** Every vendor `cmd_id`
  (`>= 0x10000`) must appear in `vendor_ioctls`, or dispatch rejects `failed_precondition`
  unconditionally — regardless of what buffers the request carries. `validate_vendor_ioctl_buffers`
  now returns `Result<VendorIoctlContract, Status>` (not `Result<(), Status>`), since a `None`
  contract can no longer reach the caller as a success; `run_vendor_ioctl` takes `VendorIoctlContract`
  by value instead of `Option<VendorIoctlContract>` for the same reason (the "unconfigured but
  forwarding" state is now unrepresentable, not just unreached).
- **No new `Bufferless`/`"none"` shape variant.** `Raw { input_bytes: 0, output_bytes: 0 }` already
  means exactly "NULL/NULL only" — `validate_vendor_ioctl_buffers`'s `Raw` branch already rejects any
  non-empty input against `input_bytes == 0` and any `output_capacity > 0` against `output_bytes ==
  0`, and `VendorIoCtl::new`/`input_ptr` already map an empty/absent buffer to `NULL`. A third variant
  would be a second spelling of an existing state, the sixth schema amendment to this one contract in
  this PR for a purely cosmetic gain. An operator who wants a genuinely bufferless vendor command
  allowlisted now writes `shape = "raw"` with both byte counts omitted (both default to `0`).
- **`VendorIoctlContract::SbyteArray` gains `input_required: bool, output_required: bool` (both
  default `false`)**, checked as outer-pointer presence: `input_required` rejects `request.input ==
  None` (NOT `!is_empty()` the way `Raw`'s check works — a present-but-empty wrapped input still
  becomes a real non-NULL `SBYTE_ARRAY` with `NumOfBytes: 0`, which already satisfies a presence
  requirement; whether an empty array is otherwise acceptable is the vendor DLL's own business, not
  this service's), `output_required` rejects `output_capacity == 0`. `VendorIoctlConfigEntry` gains
  the same two fields (`#[serde(default)]`). `parse_vendor_ioctl_entry` now startup-rejects
  `input_required`/`output_required` set alongside `shape = "raw"` (redundant with, and easily
  confused for, that shape's own `input_bytes`/`output_bytes`-implies-required convention) and
  `input_bytes`/`output_bytes` set alongside `shape = "sbyte_array"` (silently ignored before this
  amendment, now a startup error instead, the same "typo surfaces now, not later" convention as every
  other grammar check this table enforces) — both symmetric with the existing canonical-key-grammar
  and below-vendor-range checks' philosophy of rejecting a config an operator almost certainly did not
  intend, rather than silently ignoring it.
- **Deliberate behavior change, not a bug:** a vendor command that previously needed no configuration
  at all (a bufferless NULL/NULL call against an unconfigured `cmd_id`) now requires an explicit
  `vendor_ioctls` entry. A deployer who previously relied on that exemption for a trigger-style vendor
  command must now configure it as `shape = "raw"` with no byte counts.
- **Accepted residual (unchanged in kind):** a wrong-but-present `input_required`/`output_required =
  false` on a command that actually always dereferences that pointer is still undefined behavior —
  the same operator-trust class as this ADR's existing "misconfigured-but-present size/shape"
  residual above, not a new escalation.
- Test coverage: `rpc_misc.rs`'s `validate_vendor_ioctl_buffers_tests` gained cases for a bufferless
  request against an unconfigured `cmd_id` (now `failed_precondition`, inverting the prior
  `no_buffers_passes_with_no_config_regardless_of_wrapped_bit` test's assertion), a fully-optional
  `Raw{0,0}` contract accepting a bufferless request, and the four new `SbyteArray`
  `input_required`/`output_required` combinations. `tests/grpc_mock/vendor_passthrough.rs` gained two
  wrapped-mode required-direction rejection tests against a new mock-config entry (`0x00010005`,
  unregistered at the native mock), and `harness.rs::VENDOR_IOCTLS_TOML` gained a bufferless
  `0x00010000` entry so `cmd_id_at_vendor_range_floor_dispatches_through_vendor_path` (which sends a
  NULL/NULL request to the exact vendor-range floor) keeps proving that boundary reaches native
  dispatch instead of now failing precondition; the renamed
  `vendor_ioctl_null_null_on_unconfigured_cmd_id_is_failed_precondition` (previously
  `..._still_forwards`) asserts the new rejection directly using a genuinely unconfigured `cmd_id`.
- `docs/rpc-api-guide.md`'s "IO Control" section, `docs/j2534-0404-architecture.md`'s "Vendor
  IoctlID/ConfigParameterID passthrough" note, and `j2534-0404-service/docs/implementation-notes.md`'s
  Assumptions section are all updated to describe the allowlist-for-every-`cmd_id` requirement and
  the `input_required`/`output_required` fields in place of the "bufferless needs no config"
  language.
- ADR-178 is unaffected: this amendment introduces no new proto message, field, or RPC method — the
  wire-level header is unchanged; this is purely a service-side validation-policy and config-schema
  change.

**Amended 2026-09-07, PR #133 Codex review round (reject unknown `vendor_ioctls` contract fields).**
The immediately preceding amendment's new `input_required`/`output_required` fields (and the
existing `input_bytes`/`output_bytes`) are exactly the kind of safety-critical, boolean/numeric
`#[serde(default)]` field a misspelling (`input_requred`, `input_byte`) silently deserializes as
its default (`false`/`0`) rather than failing to parse — serde simply ignores an unrecognized TOML
key by default. That would let an operator believe a command's buffer is required when the typo'd
field actually left it optional, permitting exactly the NULL-pointer dispatch this whole contract
mechanism exists to prevent. `VendorIoctlConfigEntry` now carries `#[serde(deny_unknown_fields)]`,
and `vci_service_config::find_vendor_ioctls` now uses the strict config loader (mirroring
ADR-218's `validate_vendor_struct_type_keys` identical fix) instead of the lenient one every other
per-key lookup in that module uses: a `deny_unknown_fields` violation fails the WHOLE `config.toml` document
to parse, and the lenient loader would otherwise swallow that into "vendor_ioctls table absent"
rather than surfacing it as the startup error it should be. `find_vendor_ioctls`'s return type
changed from `Option<HashMap<...>>` to `Result<Option<HashMap<...>>, ConfigFileError>` accordingly;
`resolve_vendor_ioctls` propagates the new `Err` case as a startup error. Test coverage: an unknown
field in a `vendor_ioctls` entry fails to parse, and a malformed config file (not just an unknown
field) returns `Err` from `find_vendor_ioctls` rather than resolving to an absent table.
**Accepted residual (an existing, documented `vci-service-config` tradeoff, not new here):** a
malformed `vendor_ioctls` entry anywhere in `config.toml` fails the *whole* shared document to
parse, which every OTHER lenient lookup in that module (`find_library_path`,
`find_logging_config`, etc. -- and, cross-crate, `j2534-0404-registry`/`iso22900-registry`'s own
auto-discovery) then silently treats as "nothing configured" rather than a hard failure
(edge-case-hunter, PR #133 pre-merge audit). This is not a new failure class this amendment
introduces: `vci-service-config/docs/implementation-notes.md` already documents this exact
single-shared-document, lenient-by-default tradeoff as a deliberate, pre-existing design (ADR-033,
ADR-107 addendum (j)) -- a malformed `[[...modules]]` entry has had the identical whole-document
blast radius on every other lenient lookup since ADR-107, which is why `find_modules` alone
already needed its own strict-loader carve-out, the same pattern `vendor_ioctls`/
`vendor_struct_types` now also follow. `deny_unknown_fields` widens the set of things that can
trigger this pre-existing tradeoff (one more class of malformation, a misspelled key, alongside
the wrong-type/missing-field/syntax-error triggers that already existed) but does not change its
fundamental shape or newly introduce it.

**Amended 2026-09-07, PR #133 Codex review round (reject `vendor_ioctls` entries reserved by
this service's own IOCTL handlers; no design deviation).** Decision item 2's original "Accepted
residual" on this service's `PDU_IOCTL_BASE` (`0x2900_0000+`) private ids framed the collision
risk as something only a future maintainer *relocating* one of those constants into the vendor
range could create. That framing missed a second, already-live path to the identical collision:
an operator's own `vendor_ioctls` config can land on one of the already-reserved ids today, with
no relocation of anything required — e.g. configuring `vendor_ioctls."0x29000001"` (`PDU_IOCTL_RESET`).
The below-vendor-range check alone accepts it (it *is* `>= 0x10000`), so the contract loads
successfully at startup and appears active — but `rpc_io_ctl`'s dedicated `PDU_IOCTL_RESET` match
arm intercepts every request to that `cmd_id` before it ever reaches `rpc_io_ctl_vendor`, so a
client attempting the configured vendor command resets module state instead. `service.rs` gained
`is_reserved_service_ioctl_id`, a `pub(crate)` predicate listing all ~28 `PDU_IOCTL_BASE`-derived
constants explicitly (not a range check, to stay correct even if a future addition breaks the
constants' current numeric contiguity); `config::parse_vendor_ioctl_entry` now calls it right
after the below-vendor-range check and rejects a match at startup, naming the collision. Test
coverage: a unit test asserting every one of the 28 constants is rejected by the predicate, a
`parse_vendor_ioctl_entry` test using `PDU_IOCTL_RESET`'s own value, and boundary tests confirming
values immediately adjacent to the reserved range (one below `PDU_IOCTL_BASE + 0x01`, one above
`PDU_IOCTL_BASE + 0x1C`) are still accepted.

**Amended 2026-09-08, PR #133 Codex review round (bound raw-mode contract sizes before dispatch; a
design-advisor consult, since this revisits the third round's removal of raw mode's fixed
allocation cap).** Raw mode's `input_bytes`/`output_bytes` were validated only against the
below-vendor-range and reserved-id checks above -- an operator could configure a syntactically
valid but infeasible value (`output_bytes = 0xffffffff`), which would parse successfully and only
manifest at dispatch time: `AlignedByteBuf::zeroed` allocates the FULL configured size regardless
of the client's own requested `output_capacity` (a tiny 1-byte client request against a
`u32::MAX`-configured contract still attempts a ~4 GiB allocation), and `vec!` is infallible --
this is a guaranteed process abort for every client, triggered purely by the configured NUMBER,
with no native DLL involvement and no need for the value to be "wrong" relative to any real
contract the way the existing "wrong-but-present size" residual requires.

Escalated to a design-advisor consult, since a naive fix (reintroducing a fixed cap on raw mode's
allocation) would repeat exactly the third-round mistake this ADR already corrected: that cap
SUBSTITUTED a smaller allocation than configured, letting the native call proceed against an
undersized buffer -- silent corruption, not a fix. Decision: a **startup-rejection ceiling**,
`j2534-0404-service::config::VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES = 16 MiB`, applied to both
`input_bytes` and `output_bytes` in `parse_vendor_ioctl_entry`'s `"raw"` arm. This is a different
mechanism from an allocation cap: a value within the ceiling is honored EXACTLY as configured,
unchanged from before this amendment (locked in by a regression test at 100 000 bytes, well above
the removed 64 KiB cap); a value above the ceiling fails startup loudly, naming the field and
constant, instead of ever reaching dispatch. 16 MiB was chosen generously above any plausible real
vendor buffer: the largest buffer any SAE-defined J2534 object carries is `PASSTHRU_MSG.Data[4128]`
(`j2534-0404-sys/src/bindings/j2534_v0404.h`), and this workspace's gRPC services accept tonic's
default 4 MiB inbound message limit (never overridden), so no client can even deliver
`input_bytes` worth of payload anywhere near this ceiling. Raising the constant is a one-line,
reviewable change if a real vendor contract ever legitimately needs more.

**Rejected alternative: fallible allocation** (`Vec::try_reserve`/`try_reserve_exact`) at dispatch
time instead of (or in addition to) a startup ceiling. `Align16Chunk` is a user-defined newtype, so
`vec![Align16Chunk(..); n]` cannot use the allocator's zeroed-page fast path and must clone-fill
every byte; on a Linux host with the default memory-overcommit heuristic, a multi-gigabyte
`try_reserve_exact` typically SUCCEEDS at the reservation step, and the failure only surfaces when
the fill actually touches those pages -- at which point the OOM killer terminates the process
directly, never producing a catchable `Result` or an RPC error. Fallible allocation would only help
on the subset of targets/allocators that refuse the reservation up front, turns a loud startup
error an operator sees immediately into a silent per-request one they may never see, and is
inconsistent with every other infallible allocation already in this dispatch path. The
startup-rejection ceiling alone achieves the same fail-fast outcome this ADR already uses for every
other operator-config mistake, with no such gap.

**ISO22900 side left unchanged, by structural argument, not oversight.** ADR-218's vendor
STRUCTFIELD write path has a superficially similar shape (an operator-configured `entry_size`
multiplied by a count to get a buffer size) but is NOT the same hazard:
`vendor_struct_from_proto`/`convert.rs` computes `size_of_entry.checked_mul(count_of_entry)` and
REJECTS the request unless the CLIENT-SUPPLIED `vendor.value.len()` already equals that product --
the configured size is only ever compared for equality, never used as an allocation multiplier on
its own. `AlignedVendorStructBuf::from_bytes` (`iso22900/src/encode.rs`) then allocates exactly
`bytes.len()`, a copy of a payload that already exists and is already bounded by the same gRPC
inbound message limit as everything else this service accepts. The J2534 raw-mode hazard is
structurally different: there, the operator-configured size itself directly becomes the allocation
size, independent of anything the client sends. `AlignedVendorStructBuf::from_bytes` gained a doc
comment recording this divergence explicitly, so a future "keep the two `Aligned*Buf` copies in
sync" sweep does not mistakenly add a ceiling there or remove this one.

Test coverage: `output_bytes`/`input_bytes` exactly at the ceiling still accepted; one above the
ceiling rejected for each field independently; `output_bytes = u32::MAX` (the exact scenario in
Codex's finding) rejected; 100 000 bytes (well above the removed 64 KiB cap) still accepted,
locking in the third-round decision this amendment must not reverse. A `debug_assert!` was added to
`AlignedByteBuf::zeroed` asserting its `len` argument never exceeds the configured ceiling -- a
test-only safety net catching a future caller that bypasses `validate_vendor_ioctl_buffers`'s
config-sourced sizing, not a runtime guard (the ceiling is already enforced earlier, at config
parse time).
