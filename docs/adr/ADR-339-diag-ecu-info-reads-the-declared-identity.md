# ADR-339: `diag.ecu_info` Reads the Declared Identity Through a Literal Key

**Date:** 2026-10-20
**Status:** Accepted
**Affects:** `diag-frontend` (the lowering of `diag.ecu_info`, `STAR_UNKNOWN_IDENTITY`), Starlark subset 3.2, 3.4, 4.6 and 5, `csv-tables.md` 4.13; ADR-254, ADR-336, ADR-338

## Context

`diag.ecu_info()` is in the `diag` API of the Starlark subset (3.2) as a read-only call for the
variant, part number and software version, with no instruction, so the transpiler rejected it
(`STAR_API_NOT_AVAILABLE`). The first idea was a new `Op::EcuInfo` with a `DiagHost::ecu_info`
host call. Two facts make that unnecessary. The values a procedure may ask for are already
declared: `identity.csv` (`csv-tables.md` 4.13) gives the VIN, the hardware part number and the
software version each a single-DID read whose data record is the field, and every program carries
those sources (ADR-336 item 1). And ADR-338 already lowers a service source to `PushBytes`,
`SendReceive` and `DecodeField`, instructions the VM, the journal and the agent's policy know.

The call's result also needs a shape. A `diag.request` result is not a record known at
ingestion: it is the response bytes at run time, read only through a literal string key
(3.4, ADR-317), and the subset has no attribute access (2.4).

## Decision

1. **No new instruction and no host call.** `diag.ecu_info()` lowers at ingestion to the
   instructions that read the declared sources.
2. **The result is read through a literal key, applied directly to the call:**
   `diag.ecu_info()["vin"]`. The keys are `variant`, `vin`, `part_number` and
   `software_version`, in the spelling of 3.4's dict with fixed keys. The call takes no arguments
   (`STAR_UNSUPPORTED_SYNTAX` otherwise, as for the other `diag` calls). The call has no value of
   its own: a call that no literal key reads (a statement, an assignment, an argument, a
   comparison) is `STAR_NOT_LOWERABLE`; a key that is not a string literal is
   `STAR_DYNAMIC_KEY_NOT_ALLOWED`; another key is `STAR_UNSUPPORTED_SYNTAX`.
3. **`variant` is a string constant**, the short name of the variant the program is lowered for
   (ADR-325). It sends nothing and folds, so `diag.ecu_info()["variant"] == "x"` selects a branch
   at ingestion. A variant with no short name is `STAR_NOT_LOWERABLE`; the converter always sets
   one.
4. **The other keys each read one identity row**: `vin` the `vin` row, `part_number` the
   `hardware_part_number` row, `software_version` the `software_version` row. A key lowers to
   `PushBytes` of the DID read's request, `SendReceive`, then `DecodeField`, as a service source of
   `diag.precondition` does (ADR-338 item 2). The value is a `str` when the row's field is ASCII
   text and an `int` when it is an unsigned integer, the two types `identity.csv` allows. A key
   whose row the variant lacks is `STAR_UNKNOWN_IDENTITY`, a new code reported at the key; the
   example ECU declares only the VIN, so the other two keys are that error there. A row whose
   service the variant does not declare with a request without fields, or whose field is not
   readable as an integer or text, is `STAR_NOT_LOWERABLE`.
5. **Each key read is a separate request, made at the point of access.** Nothing is cached or
   read in advance, so two keys cost two requests, one key read twice costs two, and two reads of
   the same key may differ if the ECU changes in between. A procedure that wants one value keeps
   it in a variable. The conversion needs no hidden local and no record type.
6. **The reads are `Safe`** (4.6 already lists `ecu_info`): each is a read primitive, so it is
   journaled, the read-only policy allows 0x22, and a repeat after an interruption is harmless.
7. **The call reads in the session the ECU is in.** The procedure must call it in a session that
   answers the DID read, normally the default session; the call changes no session. A negative
   answer or one that does not decode ends the run as any field read without a check does, with
   a `VmError` (terminal, ADR-288), as ADR-338 item 7 says for `diag.precondition`.

## Alternatives rejected

- **`Op::EcuInfo` with `DiagHost::ecu_info`.** The host would interpret an ECU service inside
  one instruction, and a request sent that way would be journaled, policy-checked and given an
  idempotency by none of the machinery that knows `SendReceive`; every `DiagHost` would also gain
  a method, and the schema version would change, for data the tables already declare.
- **A record value with attributes, `diag.ecu_info().vin`, usable through a variable (the
  conversion-time record).** It needs a new value kind in the lowerer and its type inference,
  attribute syntax in the checker, tracking of the record through variables, parameters and
  function copies, and reads kept in hidden locals. That is a large feature for three values.
- **Attribute syntax only directly on the call.** It has this ADR's semantics, but makes a lone
  exception to 2.4's rule that only methods are called, and to 3.4's literal-key convention.
- **A run-time record value type.** A record on the VM's stack would need a new value type in
  `diag-ir` and every instruction that handles values, for no capability a literal key does not
  give.
- **Reading all declared rows at the call.** It sends requests the procedure does not use, and
  fails the call for a missing row the procedure never asked for.

## Consequences

- `diag.ecu_info` lowers. ADR-254's remark that the call waits for an instruction is out of
  date for it.
- `IR_SCHEMA_VERSION`, `Op`, `DiagHost`, `VmError` and the journal's formats do not change; the
  IR golden files do not change.
- The `identity.csv` rows have a second user (a procedure's read), besides the restart's checks.
  A variant that declares no row for a key cannot use that key.
- A procedure author who compares the VIN twice sends two reads.
