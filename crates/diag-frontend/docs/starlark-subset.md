# Starlark Subset Specification (Starlark -> IR)

Corresponds to design document 8.4. Defines the accepted syntax and conversion rules
for converting the custom format (CSV tables + Starlark) into IR. ADR-254 records why the
procedure language is Starlark.

**Premise**: No script engine is shipped on the agent. Conversion is performed server-side by
`diag-frontend`, and the output is `diag-ir` bytecode.
Runtime behavior is exactly the same as IR derived from ODX/OTX.

The base language is Starlark as defined by the language specification in the
`bazelbuild/starlark` repository. Extensions that some implementations offer behind options
(`while`, recursion, `set`, type annotations, top-level `for` and `if`) are not part of the
base language and are rejected.

---

## 1. Design Principles

| Principle | Rationale |
|---|---|
| Reject anything that cannot be resolved statically | The VM has no instructions for dynamic resolution at runtime |
| Guarantee that execution is finite | So execution never stalls inside an uninterruptible section (8.10.1) |
| Preserve determinism | So execution can be reproduced from the audit log (R156) |
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
| Literals | Integers, floats, strings, bytes (`b"..."`), booleans, `None`, lists, dicts (all static) |
| Indexing | List and bytes index access, slices, dict access with a string-literal key |
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
| Type annotations, `set` | Not base Starlark | `STAR_UNSUPPORTED_SYNTAX` |
| Dict access with a computed key `d[expr]` | Key cannot be resolved statically (list and bytes indexing are allowed) | `STAR_DYNAMIC_KEY_NOT_ALLOWED` |
| `fail` | Use `diag.fail` (4.3) so every abnormal end carries a code | `STAR_FAIL_NOT_ALLOWED` |
| `print`, `getattr`, `hasattr`, `dir` | Output goes through `diag.log`; reflection cannot be resolved statically | `STAR_BUILTIN_NOT_ALLOWED` |

### 2.4 Supported Built-ins

General: `len` `range` `int` `float` `str` `bool` `abs` `min` `max` `type`

Strings: `startswith` `endswith` `find` `upper` `lower` `strip` `format` `join`

Lists: `append` `pop` `index`

Bytes (built-in functions, since Starlark's `bytes` type has few methods):
`to_hex(b)` `from_hex(s)` `read_uint(b, offset, bits)` `write_uint(b, offset, bits, v)`
(returns new bytes; bytes have value semantics in the IR)

`sorted`, `reversed`, `enumerate`, `zip` and other functions that take or build sequences of
pairs are **not supported** in version 1. Use `for` with an index instead.

---

## 3. Diagnostic Primitive API

### 3.1 Overview

Provided as functions of the predeclared `diag` module. Each function maps 1:1 to a `diag-ir`
instruction (8.2.4).

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

### 3.2 Synchronous Calls

Internally these involve waiting. Starlark has no `async`, and none is needed: the VM maps the
calls to instructions that report a waiting outcome, so procedures are written as plain calls.

```python
def main():
    res = diag.request(0x22, {"did": 0xF190})  # Read VIN
    diag.log(1, res["fields"]["vin"])
```

### 3.3 Response Shape

A dict with fixed keys:

```python
{
    "ok": True,          # bool
    "nrc": None,         # negative response code (NRC) as int when ok is False
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

Every loop is a `for` over a finite sequence. A `range` whose bound is not a constant, and a
`for` over a value read at runtime (such as response bytes), gets a runtime limit embedded:

```python
# @maxIterations 100
for i in range(count):
    ...
```

Without the annotation, the default (1000) is applied and reported as a warning.
When the limit is reached, the VM stops with `LoopLimitExceeded`.

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
- `//` and `%` follow Starlark: floor division and a remainder with the sign of the divisor.
  The IR's integer division truncates toward zero (ADR-233), so the transpiler emits the
  correction for operands of different signs
- `/` is float division and always yields `F64`
- No implicit conversion between `int` and `float` beyond what Starlark itself does for
  mixed arithmetic; the transpiler inserts the conversion instructions explicitly

### 4.5 Specifying Section Attributes

Interruptibility (8.10.1) is specified with comment annotations. Ranges without a specification
are `interruptible: yes`.

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

`expected=<millis>` is the expected duration. It is compared with the remaining OEM
authentication time (8.10.1). A section starts and ends in the same function body, at the same
indentation.

### 4.6 Specifying Idempotency

Behavior on resume (8.2.5) is specified with annotations. When omitted, the per-API default is
used (`Safe` for reads; `CheckState` for `routine` and `flash_transfer`).

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

**All violations are listed**. Do not stop at the first one.

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
- Comment annotations (4.2, 4.5, 4.6) are read from the source with their line numbers and
  attached to the following statement, since the AST does not keep comments
- Constant folding is done during bytecode generation
- After generation, verify with a dry run on the `diag-ir` VM (vehicle access mocked) (12.1)

## 7. Verification

**Differential tests**: Import the same diagnostic procedure via ODX/OTX and via Starlark, and
confirm that the generated IR instruction sequences match. This directly verifies
replaceability (8.2).

**Golden tests**: Keep pairs of Starlark source and expected bytecode to detect regressions.
