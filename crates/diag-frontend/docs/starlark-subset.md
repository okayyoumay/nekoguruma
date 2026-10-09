# Starlark Subset Specification (Starlark -> IR)

Corresponds to design document 8.4. Defines the accepted syntax and conversion rules
for converting the custom format (CSV tables + Starlark) into IR. ADR-254 records why the
procedure language is Starlark.

**Premise**: No script engine is shipped on the agent. Conversion is performed server-side by
`diag-frontend`, and the output is `diag-ir` bytecode.
Runtime behavior is exactly the same as IR derived from ODX/OTX.

The base language is Starlark as defined by the language specification in the
`bazelbuild/starlark` repository. Extensions that some implementations offer behind options
(`while`, recursion, type annotations, top-level `for` and `if`) are not part of the base
language and are rejected, as are some base-language features this subset leaves out (2.3).

---

## 1. Design Principles

| Principle | Rationale |
|---|---|
| Reject anything that cannot be resolved statically | The VM has no instructions for dynamic resolution at runtime |
| Guarantee that execution is finite | So execution never stalls inside an uninterruptible section (8.10.1) |
| Preserve determinism | So execution can be reproduced from the audit log (8.2.3) |
| Report all violations at import time | Avoid discovering unsupported constructs at runtime |

Starlark already provides most of this: it has no `while`, no recursion, no exceptions, no
classes and no access to files, the network or the clock. The subset below removes the
remaining constructs the VM cannot represent (function values, closures, comprehensions,
`load`).

---

## 2. Supported Syntax

### 2.1 File Structure

One file is one procedure. The top level holds only constant assignments and `def` statements.
The procedure's entry point is `def main():`, which takes no parameters; the VM starts there.
A file without `main` is rejected (`STAR_NO_MAIN`).

### 2.2 Accepted

| Category | Syntax |
|---|---|
| Assignments | Plain assignment, augmented assignment (`+=` etc.) inside functions; top-level constants |
| Operators | Arithmetic, comparison, logical (`and` `or` `not`), bitwise, conditional expression (`a if c else b`), `in` / `not in` |
| Control flow | `if` / `elif` / `else`, `for ... in`, `break`, `continue`, `pass` |
| Functions | Top-level `def` with positional parameters, calls to them, `return` |
| Literals | Integers, floats, strings, bytes (`b"..."`), booleans, lists, dicts (within the limits of 2.5) |
| Indexing | Bytes index access and slices, constant-list index access, dict access with a string-literal key (2.5) |
| Built-ins | The restricted list in 2.4 |
| API | The diagnostic primitives in section 3 |

### 2.3 Rejected (error at import time)

| Syntax | Rationale | Error code |
|---|---|---|
| `lambda`, nested `def` | The VM has no environment capture | `STAR_CLOSURE_NOT_ALLOWED` |
| Functions as values (assigning, passing or returning a function) | Callee cannot be resolved statically | `STAR_FUNCTION_VALUE_NOT_ALLOWED` |
| List and dict comprehensions | Not supported in version 1 (may be added later as a lowering to `for`) | `STAR_UNSUPPORTED_SYNTAX` |
| `load` | One file = one procedure | `STAR_LOAD_NOT_ALLOWED` |
| `while`, recursion | Not base Starlark; execution must stay finite | `STAR_UNBOUNDED_NOT_ALLOWED` |
| Top-level statements other than constant assignments and `def` | The procedure runs from `main` only | `STAR_TOPLEVEL_STATEMENT_NOT_ALLOWED` |
| `*args`, `**kwargs`, default and keyword-only parameters, keyword arguments to user functions | Not supported in version 1 | `STAR_UNSUPPORTED_SYNTAX` |
| Type annotations | Not base Starlark | `STAR_UNSUPPORTED_SYNTAX` |
| `set` | No IR value type for it; not supported in version 1 | `STAR_UNSUPPORTED_SYNTAX` |
| Dict access with a computed key `d[expr]` | Key cannot be resolved statically (list and bytes indexing are allowed) | `STAR_DYNAMIC_KEY_NOT_ALLOWED` |
| `fail` | Use `diag.fail` (4.3) so every abnormal end carries a code | `STAR_FAIL_NOT_ALLOWED` |
| `print`, `getattr`, `hasattr`, `dir` | Output goes through `diag.log`; reflection cannot be resolved statically | `STAR_BUILTIN_NOT_ALLOWED` |

### 2.4 Supported Built-ins

General: `len` `range` `int` `float` `str` `bool` `abs` `min` `max` `type`

Strings: `startswith` `endswith` `find` `upper` `lower` `strip` `format` `join`

Lists: none in version 1 (lists are constants, 2.5)

Bytes (built-in functions, since Starlark's `bytes` type has few methods):
`to_hex(b)` `from_hex(s)` `read_uint(b, offset, bits)` `write_uint(b, offset, bits, v)`
(returns new bytes; bytes have value semantics in the IR).
For `read_uint` and `write_uint`, `offset` counts bytes from the start of `b`, `bits` is a
constant multiple of 8 from 8 to 56 (so every value fits a non-negative `I64`), and the bytes are
big-endian (this helper's fixed encoding). A field that is not byte-aligned, or little-endian, is read
through the declaration part's decode plan instead. An `offset` that runs past the end of `b`, or
a `v` outside 0 to 2^`bits` - 1, is an error at run time. Both are built from `IndexGet`,
`IndexSet`, shifts and `BitOr`.

String functions and `int`/`float`/`str` conversions that take a run-time value need the
instructions listed in 2.5; with constant arguments they are evaluated at ingestion.

`sorted`, `reversed`, `enumerate`, `zip` and other functions that take or build sequences of
pairs are **not supported** in version 1. Use `for` with an index instead.

### 2.5 Values at Run Time

The VM has four value types: `I64`, `F64`, `Bool` and `Bytes` (ADR-233). Starlark values map
onto them as follows; anything else exists only at ingestion.

| Starlark | At run time | Limits |
|---|---|---|
| `int` | `I64` | 4.4 |
| `float` | `F64` | |
| `bool` | `Bool` | |
| `bytes` | `Bytes` | |
| `str` | constant only, or UTF-8 `Bytes` when built at run time | A string built at run time (concatenation, `%` formatting, `str(x)`) needs the concatenation and conversion instructions the IR does not have yet |
| `list` | none | List literals are constants: a `for` over one is unrolled, and an index into one must be a constant. No mutation |
| `dict` | none | A dict literal appears only as a `params` argument (3.1), which the transpiler encodes into the request bytes. The response (3.4) is accessed only through literal key chains, which the transpiler resolves to the response bytes and the declaration part's decode plan for each field |
| `None` | none | Not supported in version 1 (`STAR_UNSUPPORTED_SYNTAX`) |

A construct whose lowering needs an instruction the IR does not have yet is rejected at
ingestion (`STAR_NOT_LOWERABLE`), the same way as a `diag` call with no instruction (3.2).

---

## 3. Diagnostic Primitive API

### 3.1 Overview

Provided as functions of the predeclared `diag` module. Each function lowers to the `diag-ir`
diagnostic primitives listed in 3.2 (8.2.4, `Op` in `crates/diag-ir/src/lib.rs`), with the
instructions that push their run-time operands. Most functions lower to one primitive;
`security_access` and `flash_transfer` expand to several.

```python
diag.request(service_id, params)        # Execute service -> response
diag.read_dtc(mask)                     # Read DTCs
diag.routine(routine_id, sub, params)   # Routine control
diag.security_access(level)             # Send seed -> receive key (online required)
diag.flash_transfer(session_id)         # Flash transfer (carries section attributes)
diag.wait(millis)                       # Wait
diag.hmi(form_id, params)               # HMI request -> input value
diag.record(template_id)                # Request input for a record template
diag.capture(back_millis)               # Capture a monitoring window
diag.log(level, message)                # Log output
diag.ecu_info()                         # Variant, part number, SW version (read-only)
diag.precondition(name)                 # Current value of an execution precondition (voltage, vehicle speed, etc.)
diag.fail(code, detail)                 # Abnormal end of the procedure
```

`params` is a dict literal with string keys.

Arguments that the instruction holds as a fixed field must be constant at ingestion and in the
range below; otherwise the call is rejected (`STAR_NON_CONSTANT_ARGUMENT` or
`STAR_ARGUMENT_OUT_OF_RANGE`). The ranges are those of the values themselves, not of the `Op`
field that stores them.

| Argument | Type and range |
|---|---|
| `service_id` | `int`, 0x00 to 0xFF (a UDS service identifier; ADR-235 item 2) |
| `mask` | `int`, 0x00 to 0xFF |
| `routine_id` | `int`, 0x0000 to 0xFFFF |
| `sub` | `int`, 0x00 to 0xFF |
| `level` | `int`, odd, 0x01 to 0xFD (the seed sub-function; the key is sent with `level + 1`) |
| `millis`, `back_millis` | `int`, 0 to 2^32 - 1 |
| `form_id`, `template_id` | `str` naming the form or record template |
| `level` of `diag.log` | `int`, 0 to 255 |
| `params` of `diag.hmi` | dict whose values are all constants (3.2) |
| `session_id` | `int` naming a flash session declared in the declaration part (`FlashSession` in `ir.fbs`); an unknown session is rejected (`STAR_UNKNOWN_FLASH_SESSION`) |

A `diag.wait` whose duration is only known at run time needs a stack-operand variant of `Wait`.

### 3.2 Lowering

The instruction set does not cover this whole API yet. Missing instructions are added by
appending `Op` variants, which keeps existing programs valid (ADR-233); until a function's
instruction exists, the transpiler rejects calls to it (`STAR_API_NOT_AVAILABLE`).

| Function | Instruction | Run-time operands |
|---|---|---|
| `diag.request` | `ServiceRequest` | request payload (bytes) built from `params` |
| `diag.read_dtc` | `ReadDtc` | none |
| `diag.routine` | `RoutineControl` | routine control payload (bytes) built from `params` |
| `diag.security_access` | `ServiceRequest`, `SecurityAccess`, `ServiceRequest` | expands to three steps: the seed request (SecurityAccess service, odd sub-function `level`), `SecurityAccess`, which turns the seed (bytes) into the key through the host, and the send-key request (sub-function `level + 1`) carrying that key; a negative response to either request ends the call like a failed `diag.request` |
| `diag.flash_transfer` | `FlashTransfer` | one block (bytes) per instruction; the call expands to a loop over the flash session's blocks |
| `diag.wait` | `Wait` | none |
| `diag.hmi` | `HmiRequest` | none: the instruction sends a constant, so the transpiler encodes the form and its constant `params` into one constant-pool entry; parameters known only at run time need a variant that takes them from the stack |
| `diag.record` | `RecordInput` | none: the template reference is a constant-pool entry |
| `diag.capture` | `MonitorCapture` | none |
| `diag.log` | `Log` | none; the message is a constant, so a message built at run time needs a log instruction that takes it from the stack |
| `diag.ecu_info` | none yet | |
| `diag.precondition` | none yet | |
| `diag.fail` | none yet | |

### 3.3 Synchronous Calls

Internally these involve waiting. Starlark has no `async`, and none is needed: the VM maps the
calls to instructions that report a waiting outcome, so procedures are written as plain calls.

```python
def main():
    res = diag.request(0x22, {"did": 0xF190})  # Read VIN
    diag.log(1, res["fields"]["vin"])
```

### 3.4 Response Shape

A dict with fixed keys:

```python
{
    "ok": True,          # bool
    "nrc": 0,            # negative response code (NRC) when ok is False, else 0
    "fields": {},        # name -> int | float | string | bytes
    "raw": b"",          # response bytes
}
```

The keys of `fields` are `Field.short_name` from the IR declaration part. They are defined on
the CSV table side, so a `fields` key is checked against the declaration part at import time.

---

## 4. Conversion Rules

### 4.1 Static Resolution

- Function calls must resolve to a top-level `def` in the same file, a built-in in 2.4 or a
  `diag` function
- Recursion, direct or indirect, is detected by building a call graph
  (`STAR_UNBOUNDED_NOT_ALLOWED`)
- Dict keys are string literals only. `d["a"]` is allowed; `d[k]` is not

### 4.2 Loop Limits

Every loop is a `for` over a finite sequence. A constant list is unrolled (2.5). A `range` whose
bound is not a constant, such as `range(len(b))` over response bytes (Starlark bytes are not
iterable themselves), gets a runtime limit embedded:

```python
# @maxIterations 100
for i in range(count):
    ...
```

Without the annotation, the default (1000) is applied and reported as a warning.
The limit is a counter the transpiler emits around the loop; when it is reached, the procedure
ends as if `diag.fail("LOOP_LIMIT_EXCEEDED", ...)` had been called at that point (3.2).

A polling loop is written as a bounded `for` with `break`:

```python
for _ in range(50):
    res = diag.routine(0xFF00, 0x03, {})
    if res["ok"]:
        break
    diag.wait(100)
```

### 4.3 Error Handling

Use return values; Starlark has no exceptions.

```python
def main():
    res = diag.request(0x22, {"did": 0xF190})
    if not res["ok"]:
        diag.log(3, "Read failed NRC=%d" % res["nrc"])
        return                       # End the procedure normally
```

`return` from `main` means normal termination of the procedure. For abnormal termination, use
`diag.fail(code, detail)`.

### 4.4 Numbers

- `int` maps to the IR's `I64`. Starlark integers are unbounded, so a value outside the 64-bit
  range is an error at run time, consistent with the IR's checked arithmetic (ADR-233)
- `//` and `%` on two `int` operands follow Starlark: floor division and a remainder with the
  sign of the divisor. The IR's integer division truncates toward zero (ADR-233), so the
  transpiler corrects the result only when the remainder is non-zero and the operands have
  different signs (`-7 // 2` is `-4`, while `-4 // 2` stays `-2`)
- `//` and `%` with a `float` operand need floor and remainder instructions for `F64` that the
  IR does not have; they are rejected (`STAR_NOT_LOWERABLE`) unless both operands are constants,
  which are folded at ingestion
- `/` is float division and always yields `F64`
- `>>` is arithmetic in Starlark, while the IR's `Shr` is logical (ADR-233). The transpiler
  lowers `x >> n` as `x >> n` for `x >= 0` and as `~((~x) >> n)` for negative `x` (the complement
  is `BitXor` with -1), which gives the arithmetic result with the existing instructions
- `<<` must not lose bits, since a Starlark integer would grow instead, but the IR's `Shl` drops
  shifted-out bits. A `<<` with a run-time operand needs an overflow-checked shift and is
  rejected (`STAR_NOT_LOWERABLE`) until one exists; constant operands are folded at ingestion
- A shift count outside 0 to 63 is an error at run time. Starlark accepts larger right shifts;
  this subset does not
- No implicit conversion between `int` and `float` beyond what Starlark itself does for
  mixed arithmetic; the transpiler inserts the conversion instructions explicitly

### 4.5 Specifying Section Attributes

Interruptibility (8.10.1) is specified with comment annotations. Ranges without a specification
are `interruptible: yes`.

Every comment whose text starts with `@` is an annotation. An annotation that is not one of the
forms in 4.2, 4.5 and 4.6, has an unknown or misspelled keyword or attribute, or leaves a
`@section` without its `@endsection` (or the reverse) is rejected at ingestion
(`STAR_BAD_ANNOTATION`), so a typo such as `# @section uninteruptible` cannot silently leave a
range interruptible.

```python
    # @section uninterruptible expected=120000
    diag.flash_transfer(1)
    # @endsection
```

| Annotation | `Interruptible` |
|---|---|
| `# @section interruptible` | `Yes` |
| `# @section uninterruptible` | `No` |
| `# @section recoveryRequired` | `RecoveryRequired` |

`expected=<millis>` is the expected duration, an integer from 0 to 2^32 - 1; a value outside that
range is rejected (`STAR_ARGUMENT_OUT_OF_RANGE`). It is compared with the remaining OEM
authentication time (8.10.1). A section starts and ends in the same function body, at the same
indentation.

### 4.6 Specifying Idempotency

Behavior on resume (8.2.5) is specified with annotations. When omitted, the per-API default is
used:

| Call | Default |
|---|---|
| `request` with a read service (ReadDataByIdentifier, ReadDTCInformation), `read_dtc`, `ecu_info`, `precondition` | `Safe` |
| `wait`, `capture`, `log` | `Safe` (no ECU state changes) |
| `hmi`, `record` | `Safe`: a repeat at the same instruction keeps its inquiry number, so the host treats it as a poll of the open inquiry, not a new one (ADR-233) |
| `security_access`, `routine`, `flash_transfer` | `CheckState` (a repeated security access starts again from a new seed request) |
| any other `request` | `Unsafe`, since a service the transpiler does not classify may change ECU state |

```python
    # @idempotency unsafe
    diag.routine(0xFF00, 0x01, {})
```

### 4.7 Source Maps

The source file line and column are recorded for each instruction and stored in
`Program.source_map`. This allows execution traces and audit logs to refer back to the original
source.

---

## 5. Error Reporting

**All subset violations are listed**. Do not stop at the first one.

A syntax error is different: the parser (6) stops at the first one and returns no AST, so a file
that does not parse reports that single error. Subset validation runs on a file that parses and
lists every violation it finds.

```json
{
  "status": "rejected",
  "errors": [
    {
      "code": "STAR_CLOSURE_NOT_ALLOWED",
      "file": "sequence.star",
      "line": 42,
      "column": 9,
      "snippet": "f = lambda x: x + 1",
      "hint": "Rewrite as a top-level def"
    }
  ],
  "warnings": [
    {
      "code": "STAR_LOOP_LIMIT_DEFAULTED",
      "file": "sequence.star",
      "line": 88,
      "hint": "Specify @maxIterations (default 1000 applied)"
    }
  ]
}
```

---

## 6. Implementation Notes

- Parse with the syntax crate of `starlark-rust` (`starlark_syntax`), using only its parser and
  AST, not its evaluator. Walk the AST to detect 2.3
- Detection uses an "allowlist approach". Unknown node kinds are rejected as
  `STAR_UNSUPPORTED_SYNTAX` so that nothing slips through by oversight
- Comment annotations (4.2, 4.5, 4.6) are taken from the comment tokens the parser's lexer
  reports, never from raw source lines, so `# @` inside a string literal is not an annotation.
  Each annotation is attached by its line to the following statement
- Constant folding is done during bytecode generation
- After generation, verify with a dry run on the `diag-ir` VM (vehicle access mocked) (12.1)

## 7. Verification

**Differential tests**: Import the same diagnostic procedure via ODX/OTX and via Starlark, and
confirm that the generated IR instruction sequences match. This directly verifies
replaceability (8.2).

**Golden tests**: Keep pairs of Starlark source and expected bytecode to detect regressions.
