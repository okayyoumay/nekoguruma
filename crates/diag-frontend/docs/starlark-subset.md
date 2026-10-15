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
remaining constructs the VM cannot represent (closures, function values other than the
capture-free functions and function arguments of 2.6, comprehensions, `load`).

---

## 2. Supported Syntax

### 2.1 File Structure

One file is one procedure. The top level holds only constant assignments and `def` statements,
and a docstring may open the file and each function. A top-level constant cannot call `diag` or
a function of the file, which run only from `main`. The procedure's entry point is
`def main():`, which takes no parameters; the VM starts there. A file without `main`, or whose
`main` takes parameters, is rejected (`STAR_NO_MAIN`).

### 2.2 Accepted

| Category | Syntax |
|---|---|
| Assignments | Plain assignment, augmented assignment (`+=` etc.) inside functions; top-level constants |
| Operators | Arithmetic, comparison, logical (`and` `or` `not`), bitwise, conditional expression (`a if c else b`), `in` / `not in` |
| Control flow | `if` / `elif` / `else`, `for ... in`, `break`, `continue`, `pass` |
| Functions | Top-level `def` with positional and default parameters, calls to them with positional and keyword arguments (2.7), `return` |
| Lambdas, nested `def` | Capture-free, bound to a name or passed as an argument (2.6) |
| Literals | Integers, floats, strings, bytes (`b"..."`), booleans, lists, dicts (within the limits of 2.5) |
| Indexing | Bytes index access and slices, constant-list index access, dict access with a string-literal key (2.5) |
| Built-ins | The restricted list in 2.4 |
| API | The diagnostic primitives in section 3 |

### 2.3 Rejected (error at import time)

| Syntax | Rationale | Error code |
|---|---|---|
| A `lambda` or nested `def` that names a local, parameter, loop variable or bound function of the enclosing function; a `lambda` or `def` inside a `lambda` or nested `def`; a nested `def` inside `if` or `for` | The VM has no environment capture | `STAR_CLOSURE_NOT_ALLOWED` |
| Functions as values outside 2.6 (returning a function, assigning it to another name, storing it in a list or dict, comparing it, passing it to a built-in or a `diag` function, using it as a default value) | Callee cannot be resolved statically | `STAR_FUNCTION_VALUE_NOT_ALLOWED` |
| List and dict comprehensions | Not supported in version 1 (may be added later as a lowering to `for`) | `STAR_UNSUPPORTED_SYNTAX` |
| `load` | One file = one procedure | `STAR_LOAD_NOT_ALLOWED` |
| `while`, recursion | Not base Starlark; execution must stay finite | `STAR_UNBOUNDED_NOT_ALLOWED` |
| Top-level statements other than constant assignments and `def` | The procedure runs from `main` only | `STAR_TOPLEVEL_STATEMENT_NOT_ALLOWED` |
| `*args`, `**kwargs`, keyword-only parameters, keyword arguments to built-ins and `diag` functions | Not supported in version 1 | `STAR_UNSUPPORTED_SYNTAX` |
| A missing, repeated or unknown argument, or too many positional arguments, in a call to a user function (2.7) | The call cannot be bound to the parameters | `STAR_ARGUMENT_MISMATCH` |
| Calls that pass functions needing more than 1,000 specialized copies (2.6) | Bounds ingestion work on untrusted input | `STAR_SPECIALIZATION_LIMIT` |
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

### 2.6 Lambdas, Nested Functions and Function Arguments

ADR-300 allows functions in two places, both resolved at ingestion.

A **capture-free function** is a `lambda`, or a `def` written directly in the body of a
top-level `def` (a nested `def`), whose body names only its own parameters and locals, top-level
constants, top-level `def`s, the built-ins of 2.4 and `diag`. Its parameters follow the rules
for top-level `def` parameters (2.7). One that names anything else from the enclosing function,
including another lambda or nested `def` bound there, is a closure and is rejected (2.3).

A function (a capture-free function or a top-level `def`) may appear only:

- **bound to a name**: a lambda assigned to a plain name, as a top-level constant or as a
  statement directly in a function body (not inside `if` or `for`), or a nested `def` (also not
  inside `if` or `for`), with no other binding of that name in the same scope and no use before
  the binding. The name may then be called or passed as an argument;
- **as an argument**: a lambda, a name bound to a function, a top-level `def` name or a function
  parameter, passed as an argument to a top-level `def` or to a bound function.

A parameter that receives a function is a **function parameter**: in its function's body it
may only be called or passed on as an argument. A parameter is a function parameter when its
body calls it or passes it to a function parameter of another function, so a helper that
only forwards a function (`def relay(f): return poll(f)`) is accepted. The transpiler finds the
function parameters from the bodies, following such forwarding chains, and checks every call
site against them; a function passed to any other parameter, or a value passed to a function
parameter, is rejected (`STAR_FUNCTION_VALUE_NOT_ALLOWED`).

```python
POLL_LIMIT = 50

def poll(done):
    for _ in range(POLL_LIMIT):
        if done():
            return True
        diag.wait(100)
    return False

def main():
    if not poll(lambda: diag.routine(0xFF00, 0x03, {})["ok"]):
        diag.fail("ROUTINE_TIMEOUT", "routine did not finish")
```

A nested `def` keeps a helper next to the one function that uses it:

```python
def main():
    def ready():
        return diag.request(0x22, {"did": 0xF186})["ok"]

    if not poll(ready):
        diag.fail("NOT_READY", "ECU did not answer")
```

**Lowering.** A bound or passed lambda and a nested `def` become unnamed top-level functions. A
call that passes functions is specialized: the transpiler emits one copy of the callee per distinct
combination of functions it receives, in which each call through a function parameter is a
`Call` to the received function; call sites passing the same functions share a copy. Recursion
(4.1) is judged by source function along the specialized call paths, so `invoke(reenter)`, where
`invoke(f)` calls `f()` and `reenter()` calls `invoke(done)`, is rejected although it reaches two
different copies of `invoke`. Specialization has a budget of 1,000 copies per file, counted before
each copy is built, so a file whose helpers multiply copies cannot exhaust the server; the call
that would exceed it is `STAR_SPECIALIZATION_LIMIT`. No IR instruction is involved.

### 2.7 Default Values and Keyword Arguments

A parameter of a user function (a top-level `def` or a capture-free function of 2.6) may have a
default value. The default must fold to a constant at ingestion (literals, top-level constants
and operators over them) and must not be a function; parameters with defaults follow those
without. A call to a user function passes its positional arguments first and may then name
parameters (`check(res, strict=True)`). `*args`, `**kwargs` and keyword-only parameters (after a
bare `*`) are not supported, nor are keyword arguments to built-ins and `diag` functions (2.3).

The transpiler binds every argument to its parameter at the call site and fills in the defaults,
so the IR call is positional. Arguments are still evaluated in source order, since an argument
can be a `diag` call: the transpiler evaluates them in that order into locals before the call. A
missing, repeated or unknown argument, or too many positional ones, is `STAR_ARGUMENT_MISMATCH`;
for a call through a function parameter (2.6) it is checked in each specialized copy.

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
| `code` of `diag.fail` | non-empty `str` constant, 1 to 64 bytes of UTF-8 (the IR accepts 1 to 128; `Program::validate` checks the constant, ADR-280) |
| `detail` of `diag.fail` | `str` expression, which may be built at run time |
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
| `diag.fail` | `Fail` | `code` as a constant-pool entry; `detail` as bytes from the stack, since it can be built at run time. The instruction ends the procedure as failed and is terminal (ADR-280) |

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
Which declared service a `diag.request`, `diag.routine` or `diag.read_dtc` call means is found
from its SID or routine identifier and the constant fields of the request (`csv-tables.md` 5.3).
Constants are part of the message prefix, so they are not keys of `fields`; neither are fields
inside a nested layout, so only the scalar fields at the top of the response are keys
(`csv-tables.md` 5.4). Selection happens at ingestion, so a `params` value for a constant's key
(such as `did`) must itself be a constant (`STAR_NON_CONSTANT_ARGUMENT`). Request parameters are
given as internal (coded) values; the declaration part's conversions apply only to decoded
response values.

---

## 4. Conversion Rules

### 4.1 Static Resolution

- Function calls must resolve to a top-level `def` in the same file, a built-in in 2.4 or a
  `diag` function. A call through a name bound to a lambda or nested `def`, or through a
  function parameter, resolves to that function or to the function each specialized copy
  receives (2.6)
- Recursion, direct or indirect, is detected by building a call graph
  (`STAR_UNBOUNDED_NOT_ALLOWED`). Calls through function parameters are followed along each
  specialized call path, and a path that enters a function while the same source function is
  already on it is recursion (2.6)
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
ends as if `diag.fail("LOOP_LIMIT_EXCEEDED", ...)` had been called at that point: the counter lowers to a `Fail` with that code (3.2).

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
  different signs (`-7 // 2` is `-4`, while `-4 // 2` stays `-2`). A remainder by -1 is 0
  without a division, so it holds for the smallest integer too, whose quotient by -1 is out of
  range
- `//` and `%` with a `float` operand need floor and remainder instructions for `F64` that the
  IR does not have; they are rejected (`STAR_NOT_LOWERABLE`) unless both operands are constants,
  which are folded at ingestion
- `/` is float division and always yields `F64`. A division by zero is an error, as in
  Starlark, although the IR's float `Div` would give an infinity or NaN: the transpiler tests the
  divisor first
- Floats are totally ordered, as in Starlark: NaN equals NaN and is greater than every other
  float. The IR's comparisons follow IEEE 754, where NaN equals nothing, so a float comparison
  tests its operands for NaN first
- `>>` is arithmetic in Starlark, while the IR's `Shr` is logical (ADR-233). The transpiler
  lowers `x >> n` as `x >> n` for `x >= 0` and as `~((~x) >> n)` for negative `x` (the complement
  is `BitXor` with -1), which gives the arithmetic result with the existing instructions
- `<<` must not lose bits, since a Starlark integer would grow instead, but the IR's `Shl` drops
  shifted-out bits. A `<<` with a run-time operand needs an overflow-checked shift and is
  rejected (`STAR_NOT_LOWERABLE`) until one exists; constant operands are folded at ingestion
- A shift count outside 0 to 63 is an error, at run time and when constants are folded.
  Starlark accepts larger right shifts; this subset does not
- No implicit conversion between `int` and `float` beyond what Starlark itself does for
  mixed arithmetic; the transpiler inserts the conversion instructions explicitly

### 4.5 Specifying Section Attributes

Interruptibility (8.10.1) is specified with comment annotations. Code outside every section is
`interruptible: yes`.

Every comment whose text starts with `@` is an annotation. An annotation that is not one of the
forms in 4.2, 4.5 and 4.6, has an unknown or misspelled keyword or attribute, or stands where its
form does not allow it is rejected at ingestion (`STAR_BAD_ANNOTATION`), so a typo such as
`# @section uninteruptible` cannot silently leave a range interruptible.

A section is a whole function (ADR-301). A `@section` comment directly before a top-level `def`
makes that function a **section function**:

```python
# @section uninterruptible expected=120000
def write_image():
    diag.security_access(0x11)
    diag.flash_transfer(1)

def main():
    write_image()
```

| Annotation | `Interruptible` |
|---|---|
| `# @section interruptible` | `Yes` |
| `# @section uninterruptible` | `No` |
| `# @section recoveryRequired` | `RecoveryRequired` |

`expected=<millis>` is the expected duration, an integer from 0 to 2^32 - 1; a value outside that
range is rejected (`STAR_ARGUMENT_OUT_OF_RANGE`). It is compared with the remaining OEM
authentication time (8.10.1) each time the function is called.

- **Placement.** The comment stands alone at the start of its line (column 1), and the next line
  of code after it is a top-level `def`; only comments and blank lines may come between. A
  `@section` after code on the same line, or inside a body or a statement that spans lines, is
  rejected. A `def` takes one
  `@section`. A nested `def` or a lambda (2.6) cannot be a section function: write the step as a
  top-level `def`, which is equivalent since neither can capture anything. There is no
  `@endsection`; the section ends where the function returns, by `return`, by the end of its
  body or by `diag.fail`.
- **Extent.** Everything that runs during a call of a section function is in the section: its
  own body, every function it calls, directly or not, and every function it receives as an
  argument and calls (2.6). The lowering gives each such function a copy that only the section
  reaches and covers that copy with the same attributes; a function also called outside the
  section keeps its own copy there. These copies count toward the specialization budget of 2.6.
- **No nesting.** A call to a section function from anything that runs inside a section,
  directly or through other functions or function arguments, is rejected at that call
  (`STAR_BAD_ANNOTATION`). A call to a section function from outside every section, as many
  times as the procedure needs, enters the section once per call.
- **Values.** A section function takes what it needs as arguments and returns what the caller
  needs, like any function: one run-time value, since the VM has no run-time lists, dicts or
  tuples (2.5). Work that needs several results of a section is done inside the section, or the
  step is split into several section functions. A loop around a section that must stop early
  tests the value the section returns.

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
source. A lifted lambda's or nested `def`'s instructions are recorded at its own position, and a
specialized copy's (2.6) at the callee's. An entry is written where the position changes and
covers the instructions after it up to the next entry (ADR-304).

### 4.8 Types at Run Time

The IR's instructions take operands of one type, and the VM has no instruction that asks a value's
type, so the transpiler infers at ingestion which types each value can have (ADR-304):

- A local has the types of every value assigned to it anywhere in its function; a parameter
  has the types of the arguments its callers pass; a call has the types its function returns.
  Only `main` and the functions it calls, directly or not, are lowered; a function nothing
  reaches never runs and is left out.
- An operator whose instruction depends on its operands' types (arithmetic, comparisons,
  shifts, indexing, truth values) needs each operand to have one type. A value that can have two
  (an `int` on one path and a `float` on another, or a parameter its callers pass both) is
  `STAR_NOT_LOWERABLE` there; write the function for one type.
- Values of different types are never equal, as in Starlark: `1 == "1"` is `False` without
  an instruction comparing them. An `int` and a `float` compare by value in Starlark, which needs
  a conversion instruction at run time.
- `None` does not exist at run time (2.5). A function that returns a value must return one on
  every path, and a function that returns none cannot be used as a value
  (`STAR_UNSUPPORTED_SYNTAX`).
- Expressions whose operands are all constants are folded at ingestion with Starlark's
  semantics, so they need no instruction. A constant may hold at most 2^20 values and bytes,
  counting each value, each byte of its strings, bytes and dict keys, and a value a list holds
  twice twice; a larger one is `STAR_NOT_LOWERABLE`.
- An index into bytes known only at run time must not be negative: without a length
  instruction the transpiler cannot count it from the end, so a negative index stops the
  procedure with an index error where Starlark would count from the end. A constant negative
  index is `STAR_NOT_LOWERABLE`.

---

## 5. Error Reporting

**All subset violations are listed**. Do not stop at the first one.

A syntax error is different: the parser (6) stops at the first one and returns no AST, so a file
that does not parse reports that single error (`STAR_SYNTAX_ERROR`, or
`STAR_UNBOUNDED_NOT_ALLOWED` when the parser stops at `while`, which it reads as a reserved word).
Subset validation runs on a file that parses and lists every violation it finds, sorted by
position. Lines and columns count from 1; the column counts characters. The snippet is the
source line; of a line longer than 160 characters it is the 160 characters around the column, so
that many violations on one long line stay small.

```json
{
  "status": "rejected",
  "errors": [
    {
      "code": "STAR_CLOSURE_NOT_ALLOWED",
      "file": "sequence.star",
      "line": 42,
      "column": 9,
      "snippet": "f = lambda x: x + offset",
      "hint": "A lambda may name only its parameters and top-level names; pass offset as a parameter"
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
  The text after the one comment marker must start with `@`, so `## @...` is an ordinary comment.
  Each annotation is attached by its line to the following statement. An annotation belongs to
  a top-level `def` when the line of its comment is indented further than `def` and comes before the next
  top-level statement, so a comment after the last statement of a body still belongs to it.
  Within it, an annotation belongs to a nested `def` (2.6) whose body holds the line in the same
  way. A comment after a statement takes that statement's line and indentation.
  `@maxIterations` takes an integer from 1 to 2^32 - 1 and must precede a `for` in the same
  function; `@idempotency` takes `safe`, `checkState` or `unsafe` and must precede a statement in
  the same function; `@section` belongs to no function, starts its line, and must have a
  top-level `def` as the next line of code (4.5).
  An integer, negative ones included, outside its range is `STAR_ARGUMENT_OUT_OF_RANGE`, any
  other fault `STAR_BAD_ANNOTATION`
- Section nesting (4.5) is found on the call graph the recursion check builds, in one pass: the
  functions reachable from any section function are marked, and a call from a marked function
  to a section function is reported, unless it closes a cycle, which is reported as recursion
  only. Calls through function parameters (2.6) are followed along the specialized call paths
- The check of 2.3 sees the syntax only. A computed key, or a slice, is found on the expressions known to be
  dicts without type inference: dict literals, the responses of `diag.request`, `diag.routine`
  and `diag.read_dtc` (3.4), a response's `"fields"` (not the values in it, nor another dict's
  `"fields"`), and names assigned from those, top-level constants included. A name assigned on
  only some paths (one branch of an `if`, the body of a `for`, which may run no pass or several,
  either operand of `and`, `or` or a conditional expression) counts as a dict after it when it is
  one on any path. A `break` carries its bindings to the loop's exit and a `continue` to the next
  pass; what follows a jump or a `return` is still checked but its bindings reach nothing. A
  parameter or a name a function assigns hides a top-level name or built-in of the same spelling,
  and is a value that cannot be called (`STAR_FUNCTION_VALUE_NOT_ALLOWED`, like a top-level
  constant), unless it is a function parameter or a name bound to a function (2.6); a top-level `def` or constant named `diag` hides the module as well. A top-level
  name is bound once, by one `def` or one constant assignment, so each has one meaning in the
  whole file; a second binding is `STAR_UNSUPPORTED_SYNTAX`. A call must name a top-level
  `def`, a name bound to a function or a function parameter (2.6), a built-in of 2.4, a string
  method of 2.4 or a `diag` function of 3.1; anything else is `STAR_BUILTIN_NOT_ALLOWED`. A
  rejected construct (a capturing `lambda` or nested `def`, a comprehension, an f-string, a
  top-level statement) is checked inside as well, so that every violation is listed. What needs the declaration part or the
  instruction set (a `fields` key, a flash session, an argument's range, an instruction the IR
  lacks) is checked by the lowering
- Constant folding is done during bytecode generation
- After generation, verify with a dry run on the `diag-ir` VM (vehicle access mocked) (12.1)

## 7. Verification

**Differential tests**: Import the same diagnostic procedure via ODX/OTX and via Starlark, and
confirm that the generated IR instruction sequences match. This directly verifies
replaceability (8.2).

**Golden tests**: Keep pairs of Starlark source and expected bytecode to detect regressions.
