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

One file is one procedure, named by its file name (any file directly in the definition directory
whose name ends in `.star`, in any case; `csv-tables.md` section 1 packages it with every
variant, ADR-307). A file within the size bound of 2.8 that is not UTF-8 is not parsed and
reports `STAR_SYNTAX_ERROR` at its first byte that is not. The top level
holds only constant assignments and `def` statements, and a docstring may open the file and each
function. A top-level constant cannot call `diag` or a function of the file, which run only
from `main`. The procedure's entry point is `def main():`, which takes no parameters; the VM
starts there: `main`'s body is the program's first instruction and ends in a `Ret`, with the other
functions after it, so nothing calls `main` (4.9). A file without `main`, or whose `main` takes parameters, is rejected
(`STAR_NO_MAIN`).

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
| Calls that pass functions, or sections, needing more than 1,000 specialized copies (2.6, 4.5) | Bounds ingestion work on untrusted input | `STAR_SPECIALIZATION_LIMIT` |
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
a `v` outside 0 to 2^`bits` - 1, is an error at run time. `bits` is a constant (a run-time
`bits` is `STAR_NON_CONSTANT_ARGUMENT`, and a constant other than 8, 16, ..., 56 is
`STAR_ARGUMENT_OUT_OF_RANGE`), so with operands known only at run time both lower to code
unrolled once per byte, built from `IndexGet`, `IndexSet`, shifts, `BitOr` and `BitAnd`. A byte
past the end of `b`, and a negative `offset`, give the VM's `IndexOutOfRange` error; a `v` that
does not fit gives `Fail` with code `UINT_OUT_OF_RANGE` and a detail that names the line. With
constant operands both are evaluated at ingestion, where an out-of-range field or value is
`STAR_NOT_LOWERABLE`.

`len` of bytes known only at run time lowers to the IR's length instruction. Every other
built-in and string method of this section that takes a value known only at run time (`abs`,
`min`, `max`, `bool`, `str`, `type`, `to_hex`, `from_hex`, `len` of a string, `find`, `upper`,
`lower`, `strip`, `startswith`, `endswith`, `format`, `join`) is `STAR_NOT_LOWERABLE`, and its
hint names the kind of instruction the IR would need first (for example a string-search
instruction for `find`).

`int` and `float` of a number known only at run time lower to the IR's conversions between
integers and floats (`int` rounds toward zero, and a NaN, an infinity or a value out of the
64-bit range is an error at run time, as in Starlark). String functions and the other
conversions that take a run-time value need the instructions listed in 2.5; with constant
arguments they are evaluated at ingestion.

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
| `bytes` | `Bytes` | A slice without a step lowers at run time to the IR's slice instruction, with Starlark's clamping of negative and out-of-range indexes; a string is sliced only when it is a constant, since its indexes count characters |
| `str` | constant only, or UTF-8 `Bytes` when built at run time | Two strings, or two bytes values, known only at run time are joined with `+` through the IR's concatenation instruction, which ends the procedure with a VM error past 16 MiB; a string built by `%` formatting or `str(x)` needs the conversion instructions the IR does not have yet |
| `list` | none | List literals are constants: a `for` over one is unrolled, and an index into one must be a constant. No mutation |
| `dict` | none | A dict literal appears only as a `params` argument (3.1), which the transpiler encodes into the request bytes. The response (3.4) is accessed only through literal key chains, which the transpiler resolves to the response bytes and the declaration part's decode plan for each field. The answer of `diag.hmi` and `diag.record` (3.4) is likewise a status byte and text read through its literal keys, never a value of its own |
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
each copy is built, so a file whose helpers multiply copies cannot exhaust the server; the copies
also hold at most 2^22 instructions together. The call that would exceed either is
`STAR_SPECIALIZATION_LIMIT`. No IR instruction is involved.

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

### 2.8 Size and Nesting Limits

A file is held to these limits before it is parsed (ADR-303), so that no file, however it is
nested, can exhaust the converter's stack:

| Limit | Bound | Error code |
|---|---|---|
| Source size | 1 MiB (1,048,576 bytes) | `STAR_SOURCE_TOO_LARGE` |
| Bracket depth: `(`, `[`, `{` and an f-string's `{` nested inside each other | 64 | `STAR_NESTING_TOO_DEEP` |
| Block depth: indented blocks nested inside each other, the function body included | 32 | `STAR_NESTING_TOO_DEEP` |
| `elif` clauses of the `if` statements enclosing a point, together | 256 | `STAR_NESTING_TOO_DEEP` |
| Expression depth: operators, keywords, accesses (`.`, an index, a call) and brackets on one path from a statement down to an operand | 512 | `STAR_NESTING_TOO_DEEP` |

The expression depth counts every operator and keyword token, a unary one too, an assignment's
`=`, each `.`, index and call, and each bracket, along the deepest path: `x = a + b` is 2 deep,
`res["fields"]["rpm"]` 3 (two indexes and their bracket), `(a + b) * c` 3. Everything an element
of an expression holds may nest under every link of that element, so a chain counts in full
around the deepest bracket in it, and a bracket's depth adds to the links around it. The items of
a list, a dict, a tuple or an argument list, which commas separate, are siblings and do not add
up; the parameters of a `lambda` are not, since its body nests under all of them. Separate
statements do not add up either, `;` included, and an `if` statement's `elif` clauses stop
counting where the statement ends; an inline body after `:` is still inside the statement.

A file past a limit is refused with one error at the token that passes it, as a syntax error is
(5). Split a deep expression with intermediate names, a long `elif` chain or a deep block with
helper functions.

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
diag.hmi(form_id, params)               # HMI request -> the operator's answer (3.4)
diag.record(template_id)                # Request input for a record template -> its answer
diag.capture(back_millis)               # Capture a monitoring window
diag.log(level, message)                # Log output
diag.ecu_info()["key"]                  # Variant, VIN, part number, SW version (read-only; a literal key reads it)
diag.precondition(name)                 # Current value of an execution precondition (voltage, vehicle speed, etc.)
diag.fail(code, detail)                 # Abnormal end of the procedure
```

`params` is a dict literal with string keys. The `params` of `diag.hmi` are a dict literal too, never a
name or a constant dict, and `diag.record` takes the template id alone.

Arguments that the instruction, or the request built at ingestion, holds as a fixed field must
be constant at ingestion and in the range below; otherwise the call is rejected (`STAR_NON_CONSTANT_ARGUMENT` or
`STAR_ARGUMENT_OUT_OF_RANGE`). The ranges are those of the values themselves, not of the `Op`
field that stores them.

| Argument | Type and range |
|---|---|
| `service_id` | `int`, 0x00 to 0xFF (a UDS service identifier; ADR-235 item 2), except RequestDownload (0x34), TransferData (0x36) and RequestTransferExit (0x37), which only `diag.flash_transfer` sends |
| `mask` | `int`, 0x00 to 0xFF |
| `routine_id` | `int`, 0x0000 to 0xFFFF |
| `sub` | `int`, 0x00 to 0xFF |
| `level` | `int`, odd, 0x01 to 0x7D (the seed sub-function; the key is sent with `level + 1`; ISO 14229-1 (2026) 9.4 gives the seed requests that range, and a sub-function's bit 7 asks the server not to answer) |
| `millis` of `diag.wait` | `int`, 0 to 2^32 - 1; may be an expression known only at run time, whose value the VM checks against that range when the wait runs (`WaitOutOfRange`) |
| `back_millis` | `int`, 0 to 2^32 - 1 |
| `form_id`, `template_id` | `str` constant of 1 to 128 bytes naming the form or record template (ADR-340 item 4): a value known only at run time is `STAR_NON_CONSTANT_ARGUMENT`, a constant of another type `STAR_UNSUPPORTED_SYNTAX`, another length `STAR_ARGUMENT_OUT_OF_RANGE`. Both calls take exactly their arguments (`STAR_UNSUPPORTED_SYNTAX` otherwise) |
| `name` of `diag.precondition` | `str` constant, one of `voltage`, `externalSupply`, `ignition`, `engine`, `vehicleSpeed` (the names `# @preconditions` takes, 4.9); another `str` is `STAR_ARGUMENT_OUT_OF_RANGE`, a value known only at run time `STAR_NON_CONSTANT_ARGUMENT` and a constant of another type `STAR_UNSUPPORTED_SYNTAX`. The name need not be in `# @preconditions` |
| `level` of `diag.log` | `int`, 0 to 255 |
| `message` of `diag.log` | `str` expression, which may be built at run time, but not from a SecurityAccess or Authentication response (below; `STAR_SECRET_IN_TEXT`) |
| `code` of `diag.fail` | non-empty `str` constant, 1 to 64 bytes of UTF-8 (the IR accepts 1 to 128; `Program::validate` checks the constant, ADR-280) |
| `detail` of `diag.fail` | `str` expression, which may be built at run time, but not from a SecurityAccess or Authentication response (below; `STAR_SECRET_IN_TEXT`) |
| `params` of `diag.hmi` | dict literal of at most 32 entries (ADR-340 item 4). A key is a string literal that is an identifier (`[A-Za-z_][A-Za-z0-9_]*`) of at most 64 bytes, unique in the dict; a computed key, or one that is not a string, is `STAR_DYNAMIC_KEY_NOT_ALLOWED` as for every dict literal (4.1), a string literal that is not an identifier `STAR_UNSUPPORTED_SYNTAX`, one past a bound, a repeated key or more than 32 entries `STAR_ARGUMENT_OUT_OF_RANGE`. A value is an `int`, `bool` or `str`, a constant or known only at run time (one inferred type, 4.8, else `STAR_NOT_LOWERABLE`); a value of another type, constant or not (`float`, `bytes`, a list, a dict, a response or an answer), is `STAR_UNSUPPORTED_SYNTAX`. The form, constants included, encodes to at most 16 KiB (`STAR_ARGUMENT_OUT_OF_RANGE`) (3.2) |
| `params` of `diag.request` | dict literal whose keys and values select one declared service of the variant and give its other request fields (3.4); a call that selects none, or more than one, is rejected (`STAR_UNKNOWN_SERVICE`). The keys, and the values of the service's constants, are known at ingestion (`STAR_NON_CONSTANT_ARGUMENT`); a field's value may be known only at run time (below) |
| `params` of `diag.routine` | dict literal whose keys and values give the routine service's own constants and other request fields, as for `diag.request`; a call whose `routine_id`, `sub` and `params` select no generated routine service is rejected (`STAR_UNKNOWN_SERVICE`) |
| `session_id` | `int` naming a flash session declared in the declaration part (`FlashSession` in `ir.fbs`); an unknown session is rejected (`STAR_UNKNOWN_FLASH_SESSION`) only for a plan's session, the `@flashPlan` annotation's; the call must be a plan function's one transfer (4.9), and its `session_id` any constant expression, which the lowering evaluates and compares with the annotation's (`STAR_PLAN_SHAPE` for another session, `STAR_NON_CONSTANT_ARGUMENT` for a value known only at run time). Under the target contract of 3.2 (ADR-336) it must name a session of the target whose handle the call is made on. The call's value is the RequestTransferExit response in the 3.4 shape without `fields` (reading one is `STAR_NOT_IN_PROFILE`) |

A `diag.wait` whose duration is known at ingestion lowers to `Wait`, with the range checked then. One whose
duration is an `int` computed at run time lowers to the code that pushes it, then `WaitFromStack` (ADR-233),
which stops the run with `WaitOutOfRange` for a value outside the range; a run-time value of another type is
rejected as a constant of that type is (`STAR_UNSUPPORTED_SYNTAX`).

A request field's value of `diag.request` or `diag.routine` that is known only at run time is encoded when the
call runs. The field must be a fixed-length `uint` (another field, `STAR_NOT_LOWERABLE`) and the value an `int`
(another type, `STAR_UNSUPPORTED_SYNTAX`; one with several possible types, `STAR_NOT_LOWERABLE`). The rest of the
request is encoded at ingestion with 0 in the field. When the call runs, its values are computed in source order,
then each is checked against its field's width: a negative value, or one that does not fit, ends the procedure with
`Fail` code `PARAM_OUT_OF_RANGE` and a detail that names the param, its width and the line, before the request is sent (a `diag` call inside a value has run by then).
The values' bits are then written into the request bytes as the ingestion-time encoding places them (ADR-308), and
the instruction sends them (ADR-341).

A `diag.log` whose message is a constant `str` lowers to `Log`, which takes it from the constant pool. One whose
message is a `str` built at run time lowers to the code that pushes it, then `LogFromStack` (ADR-233); the VM
decodes invalid UTF-8 in it lossily, so a log never stops a run. A message of another type, `bytes` included, is
rejected whether it is a constant or computed at run time (`STAR_UNSUPPORTED_SYNTAX`); the level stays a constant.

**No secret in a message or detail (ADR-348 item 6).** A `diag.log` message is written to the job's communication
log, and a `diag.fail` detail to its journal and result, while the communication log keeps the bytes of a
SecurityAccess or Authentication response as their length only (design 8.11). So a message or detail that may hold
such bytes is rejected (`STAR_SECRET_IN_TEXT`). The lowering marks as secret the result of `diag.security_access`
and of a `diag.request` of SID 0x27 or 0x29 (SecurityAccess and Authentication, ISO 14229-1 (2026) 9.4 and 9.6), and
its `raw` and `fields`, with every field read from them. The mark follows the value through variables, function
parameters and returned values (a variable or parameter is marked when any value assigned or passed to it is,
4.8), and through every operator, index, slice and built-in that takes a marked operand; a conditional expression
is marked when either branch is. A response's `ok` and `nrc` are not marked, so a procedure logs whether the ECU
accepted the step and which code it refused it with. The mark follows data only, not the conditions around it: a
constant message logged in a branch taken on a secret value is accepted. A message or detail that may hold such
bytes is `STAR_SECRET_IN_TEXT` whatever its type, before its type is checked. Since a function's parameters and result
have one type for all its calls (4.8), a helper that one caller passes a secret is marked for every call: a
`diag.log` inside it is rejected there, and so is a message another caller builds from its result.

### 3.2 Lowering

Every function of this API has an instruction since ADR-340. An instruction a later function
needs is added by appending an `Op` variant, which keeps existing programs valid (ADR-233);
until it exists, the transpiler rejects calls to that function (`STAR_NOT_LOWERABLE`).

| Function | Instruction | Run-time operands |
|---|---|---|
| `diag.request` | `ServiceRequest` | request payload (bytes): encoded at ingestion from `params` into a constant-pool entry and pushed before the instruction (ADR-313). The call's value is the response (3.4), kept as its bytes (ADR-317); a call used as a statement drops it. A `params` value known only at run time under a field's key is encoded at run time (below) |
| `diag.read_dtc` | `SendReceive` | the complete request (bytes), built at ingestion from the profile's template with the `mask` constant, of the declared service the template selects (ADR-309 item 4, ADR-314 item 2); the call's value is the response, read under the profile's names (3.4) |
| `diag.routine` | `RoutineControl` | the rest of the request after the sub-function and the routine identifier (bytes), built at ingestion from `params`, with a field's value known only at run time written into it then (below); the instruction holds `routine_id` and `sub`, which select the variant's generated routine service through the profile (ADR-309 item 4, ADR-314). The call's value is the response, read under the response's declared names (3.4) |
| `diag.security_access` | `ServiceRequest`, `SecurityAccess`, `ServiceRequest` | expands to the profile's three steps (ADR-309 item 4, ADR-314): the seed request (on UDS the SecurityAccess service with the odd sub-function `level`), `SecurityAccess`, which turns the seed into the key through the host, and the send-key request (on UDS sub-function `level + 1`) carrying that key. The profile decodes neither response through a declared service, so both requests are `ServiceRequest`s of the template's SID with the payload it gives, and the variant needs no declared service for them; the seed is the seed response after the profile's leading bytes (on UDS the response SID and the level). A negative seed response ends the call before the key step, like a failed `diag.request`, and is its value. On UDS a seed whose bytes are all zero means the level is already unlocked (ISO 14229-1 (2026) 9.4.1): the call ends there too, without the key step or the send-key request, and its value is the positive seed response (ADR-405). The check is a byte loop of about 19 VM steps per seed byte plus 15 fixed, which counts against the step budget as the flash block loop does. A seed response with no seed bytes is not a zero seed and ends the procedure with `Fail` code `SEED_EMPTY` and a detail that names the line. Otherwise the value is the send-key response, positive or negative. A dry run answers the two requests by their place around `SecurityAccess` when no declared service does (6, ADR-390) |
| `diag.flash_transfer` | `FlashTransfer` | `PushBytes`, `ServiceRequest` (0x34, then 0x37), `FlashBlock`, `FlashTransfer` and a counter loop (ADR-336), only inside a flash plan function (4.9): the call expands to RequestDownload, a loop of `FlashBlock` and `FlashTransfer` over the session's one segment, and RequestTransferExit, whose run-time operands are the block index for `FlashBlock` and the block bytes it leaves for `FlashTransfer` (the contract is below). Outside a plan function the call is `STAR_PLAN_SHAPE`, and in a variant whose application layer is not UDS `STAR_NOT_IN_PROFILE` |
| `diag.wait` | `Wait`, or `WaitFromStack` | none for a constant duration; for a duration known only at run time, the duration in milliseconds (`int`), pushed before `WaitFromStack` |
| `diag.hmi` | `HmiRequest`, or `HmiRequestFromStack` | the transpiler encodes the form id and the `params` into one constant-pool entry, an `HmiTemplate` (ADR-340 items 1, 2). A call whose `params` are all constants lowers to `HmiRequest`, which takes no operand: the host receives the form as encoded. A value known only at run time is a slot of the template, its code emitted in source order before `HmiRequestFromStack`, which pops one operand per slot (the first slot deepest), fills the slots and sends the completed form. The call's value is the answer (3.4) |
| `diag.record` | `RecordInput` | none: the template reference is a constant-pool entry holding the template id with no params. The call's value is the answer (3.4) |
| `diag.capture` | `MonitorCapture` | none |
| `diag.log` | `Log`, or `LogFromStack` | none for a constant message; for a message built at run time, the message (`str`, as bytes), pushed before `LogFromStack` |
| `diag.ecu_info` | none; `PushBytes`, `SendReceive`, `DecodeField` per read key | no new instruction (ADR-339). The call has no value of its own: it is read through a string-literal key applied directly to it, `diag.ecu_info()["vin"]`, with the keys of 3.4's dict-with-fixed-keys spelling. `variant` is the short name of the variant the program is lowered for, a `str` constant that sends nothing. `vin`, `part_number` and `software_version` each read the variant's `identity.csv` row of kind `vin`, `hardware_part_number` and `software_version` (`csv-tables.md` 4.13), lowered as a service source of `diag.precondition` is: the request (bytes) is built at ingestion and pushed before `SendReceive`, then `DecodeField` reads the field. The value is a `str` for an ASCII field and an `int` for an unsigned integer one. **Each key read is a separate request, made at the point of access**: nothing is cached or read in advance, so two reads of a key may differ if the ECU changes in between; keep a value in a variable to use it twice. A key whose row the variant lacks is `STAR_UNKNOWN_IDENTITY`; a call that no literal key reads (a statement, a variable, an argument), is `STAR_NOT_LOWERABLE`; a key that is not a string literal is `STAR_DYNAMIC_KEY_NOT_ALLOWED`; another key, or an argument, is `STAR_UNSUPPORTED_SYNTAX`; a row whose service the variant does not declare with a request that has no fields is `STAR_NOT_LOWERABLE`. The reads are `Safe` (4.6). A key read must be made while the ECU is in a session that answers the DID read (normally the default session); a negative answer or one that does not decode ends the run as a field read without a check does, with a `VmError`, which is terminal |
| `diag.precondition` | `ReadInput`, or `PushBytes`, `SendReceive`, `DecodeField` | none for a runtime input: `ReadInput { input }` pushes the value as an `int` (ADR-338). For a service source, the request (bytes) is built at ingestion and pushed before `SendReceive`, then `DecodeField` reads the field from the answer. Which of the two is chosen at ingestion from the default-session source of the variant's `preconditions.csv` row of that kind (`csv-tables.md` 4.14); the programming-session source is not used. The call's value is an `int`: millivolts, 0 or 1 for a flag, km/h, or the integer the field holds. A kind with no row, or a row with no default-session source, is `STAR_UNKNOWN_PRECONDITION`; a service source whose field does not decode to an integer (a numeric conversion gives a float), or whose service the variant does not declare with a request that has no fields, is `STAR_NOT_LOWERABLE`. The call reads the row's default-session source, so a service-backed one must be called while the ECU is in a session that answers that read (normally the default session). A runtime input the agent cannot establish is a host error, which leaves the state untouched, and the job can be resumed; a service source is a read like `diag.request`'s: a negative answer or an answer that does not decode ends the run as a field read without a check does, with a `VmError`, which is terminal |
| `diag.fail` | `Fail` | `code` as a constant-pool entry; `detail` as bytes from the stack, since it can be built at run time. The instruction ends the procedure as failed and is terminal (ADR-280) |

**Target contract (ADR-309).** The table describes the lowering as built; the protocol-neutral calls `diag.read_dtc`, `diag.routine` and `diag.security_access` go through their profile. ADR-309 changes it: `diag.read_dtc`, `diag.routine` and `diag.security_access` name an intent and are lowered through the application-layer profile of the variant's protocol into that variant's declared services; `diag.read_dtc` lowers to a generic send-receive instead of `ReadDtc`; `diag.routine` keeps `RoutineControl` on UDS and uses the generic send-receive on other application layers; `diag.flash_transfer` stays UDS-only. UDS is the first profile. A profile is typed data shipped with the frontend (`diag_frontend::profile`, ADR-314): per call, the steps it expands to, each with a request template that selects the declared service by value and position and a map from the response to the result. The variant's protocols name its application layer (`csv-tables.md` 4.3), and a call to `diag.read_dtc`, `diag.routine` or `diag.security_access` that its profile does not define is rejected (`STAR_NOT_IN_PROFILE`). The UDS profile defines all three. `diag.read_dtc` is lowered: a variant with no single declared service its template selects is `STAR_UNKNOWN_SERVICE`, and one whose selected response does not fit the profile's map is `STAR_NOT_IN_PROFILE`, naming the field. `diag.routine` is lowered the same way: a variant without exactly one generated routine service its identifier, sub-function and `params` select is `STAR_UNKNOWN_SERVICE`. `diag.security_access` is lowered through the profile's templates alone (3.2 above), so it selects no declared service.

**Lowering of `diag.flash_transfer` (ADR-336).** The converter lowers `diag.flash_transfer(session)` only inside a plan function (4.9); anywhere else the call is `STAR_PLAN_SHAPE`. The call expands to a `ServiceRequest` of RequestDownload (0x34) whose operand is built from the session's segment (the two format bytes from `flash_segments.csv`, then the start address and length big-endian in the widths the format byte gives, ISO 14229-1 (2026) 14.2.2.3; a value that does not fit, a width of 0 in either nibble of the format byte is refused, and the table refuses it too, is `STAR_NOT_LOWERABLE`; a width above 8 bytes is padded with zeros). A `for` loop over `ceil(length / block_size)` blocks follows, a constant count that must not exceed 2^32 - 1, the largest block number the write-job journal records (a segment with more blocks is `STAR_NOT_LOWERABLE`), each pass `LoadLocal i; FlashBlock; FlashTransfer`. A `ServiceRequest` of RequestTransferExit (0x37, the service id alone) ends it. `Op::FlashBlock { session, segment }` pops an `int` block index and pushes that block's bytes, which the VM takes from the host's `artifact_bytes(offset, len)`; the agent has verified the artifact's hash at job check (design 8.2.5, 11.3). The block is the segment's `block_size` payload, the last block shorter when the length does not divide. The RequestDownload response is dropped: a negative one makes the first `FlashTransfer` a host error, which interrupts the plan: a plan that allows a restart there (`recovery=never`, or a recovery-required point after the RequestDownload) redoes it within `maxResumes`, and with the default point at the erase the job ends in on-site intervention. A procedure of a variant whose application layer is not UDS is rejected (`STAR_NOT_IN_PROFILE`). A session with more than one segment, or with none, is `STAR_NOT_LOWERABLE`, and so is a longest block, `min(block_size, length)`, above `MAX_BYTES_LEN - 2` (16 MiB less 2: the TransferData request adds two bytes to a block). The loop's `FlashTransfer` operand `block` is 0 on every pass: the host numbers the blocks itself (ADR-250). The expansion's one loop counter is a local of the plan. Each block costs 12 VM steps (the loop test, `FlashBlock`, `FlashTransfer` and the counter update), so the agent's default limit of 1,000,000 steps per run carries about 83,000 blocks: a larger segment needs a higher limit or several runs. The lowering does not check this, since the limit belongs to the job.

### 3.3 Synchronous Calls

Internally these involve waiting. Starlark has no `async`, and none is needed: the VM maps the
calls to instructions that report a waiting outcome, so procedures are written as plain calls.

```python
def main():
    res = diag.request(0x22, {"did": 0xF190})  # Read VIN
    if res["ok"] and res["fields"]["vin"] == "WVWZZZ1JZXW000001":
        diag.log(1, "known vehicle")
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

The keys of `fields` are `Field.short_name` from the IR declaration part. Under ADR-309 the result of `diag.read_dtc`, `diag.routine` and `diag.security_access` instead uses the names the application-layer profile maps the response to (ADR-314): `diag.read_dtc` gives `mask` (the availability status mask, an `int`) and `dtcs` (a list of dicts with `code` and `status`, both `int`), whatever the layer; `diag.routine` gives the declared names of its last step's response, since routine results are routine-specific; `diag.security_access` gives no `fields` (reading one is `STAR_NOT_IN_PROFILE`), and its `raw` is the send-key response, or the seed response that ended the call (negative, or positive with a zero seed); `raw[1] == level` tells an already unlocked level (the seed's sub-function) from a fresh unlock (the key's, `level + 1`). The profile maps a shared name to a position among the declared response's top-level fields, so a definition keeps its own names; `dtcs` is read record by record (below), while another name is `STAR_NOT_IN_PROFILE`. A `diag.request` result keeps the declared names. They are defined on
the CSV table side, so a `fields` key is checked against the declaration part at import time.
Which declared service a `diag.request`, `diag.routine` or `diag.read_dtc` call means is found
from its SID or routine identifier and the constant fields of the request (`csv-tables.md` 5.3).
Constants are part of the message prefix, so they are not keys of `fields`; only the fields
at the top of the response are keys (`csv-tables.md` 5.4), and of those the fields that hold a
nested layout cannot be read yet. Selection happens at ingestion, so a `params` value for a constant's key
(such as `did`) must itself be a constant (`STAR_NON_CONSTANT_ARGUMENT`). Request parameters are
given as internal (coded) values; the declaration part's conversions apply only to decoded
response values.

**Lowering (ADR-317).** At run time a response is the bytes the host returned, and each key is
lowered where it is read: `raw` is those bytes, `ok` is false only when the first byte is the
marker of the negative-response shape the variant's protocols declare (`7F` on UDS), and `nrc`
is the byte at that shape's code position (the third on UDS) of a negative response and 0
otherwise (ADR-309 item 4). A negative response too short to hold its code stops the run when
`nrc` is read. Another string key is `STAR_UNSUPPORTED_SYNTAX`. A response used other than
through its keys (its truth value, an operator such as `==`, a built-in) is
`STAR_NOT_LOWERABLE`.

**Field reads (ADR-320).** A response keeps the declared service of the request it came from,
and `res["fields"]` is a value of its own with the same service, read only by a string
literal. `res["fields"]["name"]` decodes the top-level field of that service's positive
response whose short name is `name` (`Op::DecodeField`, ADR-316). Its type comes from the
field's declaration: `int` for a number, also under a text table; `float` for a float or a
numeric conversion; `str` for text; `bytes` for bytes. A name the response does not declare is
`STAR_UNSUPPORTED_SYNTAX`. These are `STAR_NOT_LOWERABLE`:
- a field the procedure cannot read (a nested layout, a bytecode conversion, a text table on a
  value that is not an integer, a numeric conversion of text or bytes);
- a value that can hold responses to different services: a variable assigned from requests to
  different services anywhere in its function, also one after the other, or a parameter whose
  callers pass them. Types are inferred per variable, not per statement (4.8), so each
  response whose fields are read is kept in a variable of its own.

Reading a field of a negative response stops the run, so a procedure tests `res["ok"]` first.

**DTC records (ADR-323).** `res["fields"]["dtcs"]` is the response's DTC records, kept as the
bytes they are:

```python
res = diag.read_dtc(0x08)
if res["ok"]:
    dtcs = res["fields"]["dtcs"]
    # @maxIterations 1023
    for i in range(len(dtcs)):
        dtc = dtcs[i]
        if dtc["status"] & 0x08:
            diag.log(1, "confirmed")
```

- Reading `dtcs` stops the run when the response is negative, answers another service, or ends
  in a record cut short, as a field read does.
- `len(dtcs)` is the number of records, and `dtcs[i]` the `i`-th one, by an `int`. An index
  outside the records, a negative one included, stops the run.
- A record has `code` and `status`, both `int`, read by those literal keys; another key is
  `STAR_NOT_IN_PROFILE`.
- These are `STAR_NOT_LOWERABLE`: `for d in dtcs` (loop over `range(len(dtcs))` instead), the
  truth value of the records or a record, and a comparison of either.
- Each `res["fields"]["dtcs"]` decodes the response again, so store the records once before a
  loop rather than reading them in it. The loop limit in the example allows 1023 records, the
  most a 4095-byte response holds.

**HMI answers (ADR-340).** The value of `diag.hmi(...)` and `diag.record(...)` is the operator's
answer: one status byte, then the answer's text. It is read only through three literal keys,
each lowered where it is read:

- `ans["ok"]` is a `bool`: true when the status says the operator answered. A declined form or
  record is false.
- `ans["timed_out"]` is a `bool`: true when the HMI timeout expired before an answer (design
  5.6). A decline and a timeout both read `ok` as false, and only a timeout reads `timed_out`
  as true.
- `ans["value"]` is a `str`: the text after the status byte, empty for a decline and a
  timeout. What the text holds is the server's to fill by the kind of form: empty for a
  confirmation, the entered text for a text input, the number as decimal text for a number
  input (there is no key that reads it as an `int`), the chosen option's value for a choice,
  and for a record template the judgment of its pass criteria, `ok` or `ng`, or empty when it
  has none. Compare it with `==` against string constants.

The reads are total: an empty answer, or one whose first byte is not the answered status, reads
`ok` as false. Another string key is `STAR_UNSUPPORTED_SYNTAX`, a key that is not a string
literal `STAR_DYNAMIC_KEY_NOT_ALLOWED`, and any other use of the answer (its truth value, an
operator, a built-in) `STAR_NOT_LOWERABLE`; as a `diag.hmi` param it is
`STAR_UNSUPPORTED_SYNTAX` (3.1). A procedure tests `ans["ok"]` and
usually ends with `diag.fail` when the operator declined:

```python
ans = diag.hmi("confirm_ignition_off", {"title": "Switch the ignition off", "attempt": n})
if not ans["ok"]:
    diag.fail("OPERATOR_DECLINED", "the operator did not confirm")
```

A run whose HMI timeout fired never ends as completed, so a procedure that tests `timed_out`
only decides what it does before the run ends (ADR-340 item 6). The call waits for the
answer: it is a diagnostic primitive, `Safe` by default (4.6), with its own one-instruction
section.

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

Without the annotation, the default (1000) is applied and reported as a warning
(`STAR_LOOP_LIMIT_DEFAULTED`).
The limit is a counter the transpiler emits around the loop. It allows exactly that many
iterations; at the next one the procedure ends as if `diag.fail("LOOP_LIMIT_EXCEEDED", ...)` had
been called at that point: the counter lowers to a `Fail` with that code (3.2), whose detail
names the loop's line and limit.

How loops are lowered (ADR-305):

- A loop over a constant list, or over a constant dict (its keys), is unrolled. Each iteration
  binds the loop variable to its item at ingestion, so the body folds with it; for example, a
  constant `did` from a table can be passed where a constant is required. Items that can exist at
  run time are also stored in the variable, which holds the last item after the loop. When the
  items are lists or dicts, the variable has a value only inside the body: assigning it there,
  or using it anywhere in the function outside the loop, is `STAR_NOT_LOWERABLE`.
- Unrolling is bounded: a procedure may unroll at most 2^20 iterations, statements,
  expressions and call arguments in all, nested loops multiplying and a body counting once per copy, and each 64
  bytes of a name an unrolled copy handles counting as one more; past that it is
  `STAR_NOT_LOWERABLE`.
- A loop over `range(stop)`, `range(start, stop)` or `range(start, stop, step)` runs without
  unrolling. Its step must be an `int` constant other than 0; its start and stop are `int`s,
  evaluated once before the loop. The counter applies when the start or stop is known only at
  run time, or when `@maxIterations` precedes the loop; a loop with constant bounds and no
  annotation has none. `@maxIterations` before an unrolled loop has no effect.
- A `range` that would step past the end of the 64-bit range ends there, since the next value
  would be past its stop anyway.
- `break` leaves the innermost loop; `continue` starts its next iteration.

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
  shifted-out bits. A `<<` with a run-time operand lowers to the IR's `ShlChecked`, which stops
  the run with an overflow when the result does not fit 64 bits, where Starlark would grow;
  constant operands are folded at ingestion under the same rule
- A shift count outside 0 to 63 is an error, at run time and when constants are folded.
  Starlark accepts larger right shifts; this subset does not
- No implicit conversion between `int` and `float` beyond what Starlark itself does for
  mixed arithmetic: `+`, `-`, `*` and `/` with an `int` and a `float` operand, and `/` of two
  `int`s, compute in floats, and the transpiler inserts the conversion of each `int` operand.
  An `int` past 2^53 rounds to the nearest float, as it does in Starlark

### 4.5 Specifying Section Attributes

Interruptibility (8.10.1) is specified with comment annotations. Code outside every section is
`interruptible: yes`.

Every comment whose text starts with `@` is an annotation. An annotation that is not one of the
forms in 4.2, 4.5, 4.6 and 4.9 (the last are the contract of ADR-336), has an unknown or misspelled keyword or attribute, or stands where its
form does not allow it is rejected at ingestion (`STAR_BAD_ANNOTATION`), so a typo such as
`# @section uninteruptible` cannot silently leave a range interruptible.

A section is a whole function (ADR-301). A `@section` comment directly before a top-level `def`
makes that function a **section function**:

```python
# @section uninterruptible expected=120000
def write_vin():
    diag.security_access(0x11)
    diag.request(0x2E, {"did": 0xF190, "vin": "WVWZZZ1JZXW000001"})

def main():
    write_vin()
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
  section keeps its own copy there. These copies count toward the specialization budget of 2.6,
  in copies and in instructions: the call that would pass it is `STAR_SPECIALIZATION_LIMIT`. The
  in-place expansions of a flash plan's calls (4.9) count toward the same budget.
- **Idempotency.** A section says nothing about whether its steps may be repeated: that is an
  attribute of each `diag` call (4.6). The lowering cuts each covered copy's range around its
  diagnostic primitives. A primitive's one-instruction range carries the call's idempotency. The
  pieces between, which cover no primitive, carry `CheckState`, meaning "not stated", which the
  job runner judges as code outside every section (ADR-329, ADR-332). Every piece carries the
  section function's interruptibility and expected duration, so a copy's pieces are contiguous,
  and a section is entered only by the section function's call. The exception is `main`'s own
  section, which covers the first instruction and is entered at the start of the run; that start
  is an interruptible point, so a cancel set before the first step is taken there (the job runner
  checks it before the first step of a run from the program's start, ADR-337). Without `expected=`, the expected
  duration is 0.
- **Plan functions.** A function may carry `@section` and `@flashPlan` (4.9) together, and a plan
  function must: its `@section` is `uninterruptible` or `recoveryRequired`, since design 8.10.1
  makes the range from the erase to the completed write uninterruptible (`STAR_PLAN_SHAPE`). A section
  function that a plan function reaches is not called there: it is expanded in place, entered by
  fallthrough at its first piece (ADR-336 item 3; this changes the entry rule above for that case).
  The plan function's own section covers the whole expansion, inlined code included; an inlined
  `interruptible` piece does not lift it, since a cancel is held wherever any covering section is
  not `interruptible` (ADR-330).
- **No nesting.** A call to a section function from anything that runs inside a section,
  directly or through other functions or function arguments, is rejected at that call
  (`STAR_BAD_ANNOTATION`). A call to a section function from outside every section, as many
  times as the procedure needs, enters the section once per call. A flash plan's expansion
  (4.9) is the exception: what the plan reaches is inlined in it, so a call there nests in no
  section; a section function reached from the top level or `main` without passing through a
  plan, or from no plan at all, still nests the sections it calls.
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
| `request` with a read service (ReadDataByIdentifier, ReadDTCInformation), DiagnosticSessionControl (0x10) or TesterPresent (0x3E), `read_dtc`, `ecu_info`, `precondition` | `Safe` (a service-backed `precondition` is a read; a runtime-input one, `ReadInput`, is no diagnostic primitive and has no section of its own, and inside a section function the assembly covers it with a `CheckState` piece; a repeat of a session change or a tester-present re-enters the wanted session and behaves in any residual state, ADR-336 item 8) |
| `wait`, `capture`, `log` | `Safe` (no ECU state changes) |
| `hmi`, `record` | `Safe`: a repeat at the same instruction keeps its inquiry number, so the host treats it as a poll of the open inquiry, not a new one (ADR-233) |
| `security_access`, `routine`, `flash_transfer` | `CheckState` (a repeated security access starts again from a new seed request) |
| any other `request` | `Unsafe`, outside a flash plan's expansion (below), since a service the transpiler does not classify may change ECU state |

**ADR-336 item 8.** A `request` with DiagnosticSessionControl (0x10) or TesterPresent (0x3E) is `Safe` everywhere, as the table says (ADR-335); `# @idempotency` still replaces it. Inside a plan function's expansion (4.9), an unannotated `request` of any other service that is not a read is `CheckState`, "not stated": the plan's redo rules govern a repeat there, and `Unsafe` would refuse every plan that changes session or resets the ECU after the transfer. `# @idempotency unsafe` still forces a refusal (`Program::validate` refuses it when the plan allows a restart, since the restart replays it).

```python
    # @idempotency unsafe
    diag.routine(0xFF00, 0x01, {})
```

Every `diag` call that lowers to a diagnostic primitive carries an idempotency: the annotation's,
or the default above. `request`'s default follows its service identifier, which is a constant
(3.1). The lowering emits one `Section` over the primitive instruction alone, `pc..pc + 1`, not
over the instructions that set up its operands. Outside every section it is `interruptible: yes`
with an expected duration of 0. Inside a section function it carries that function's
interruptibility and expected duration, and the function's range is cut around it (4.5). In a
lowered program the sections therefore never overlap, and each diagnostic primitive is covered by
exactly one (ADR-332). The exception is a plan function's expansion (4.9, the contract of
ADR-336): the plan's own section covers the whole expansion, so the pieces of a section function
inlined in it, and the one-instruction sections of its `diag` calls, lie inside that section and
keep their own attributes; the readers combine covering sections conservatively (ADR-332 item 5,
ADR-330).

`Safe` means that a repeat is harmless and that the primitive behaves correctly in any session
and security state the procedure can leave the ECU in: its default session after a timeout, or
the session and unlock the procedure had entered. A restarted agent resumes a plain procedure in
whatever state the ECU is in, and only when everything after its last write is `Safe`
(ADR-335). The defaults above treat a read as working in either state, so a read that the ECU
serves only in a non-default session or after an unlock needs `# @idempotency unsafe`.

- **Extent.** An annotation applies to every diagnostic primitive its statement lowers to,
  including those of the nested statements of an `if` or a `for`. An annotation on a
  nested statement takes precedence there. It does not reach:
  - the functions the statement calls, which keep their own annotations and defaults;
  - a nested `def` or lambda, which is a function of its own (2.6).
- **No silent annotation.** A statement that lowers to no diagnostic primitive cannot be
  annotated (`STAR_BAD_ANNOTATION`), so an annotation never does nothing.
- **Copies.** A specialized copy (2.6, 4.5) carries its original's annotations.
- **Placement.** The annotation stands on a line of its own, directly before a statement of the
  same function; a comment after code on the same line is rejected (`STAR_BAD_ANNOTATION`), as is
  a second `@idempotency` before the same statement. An annotation before an `if` covers its
  `elif` and `else` parts, which are nested statements of the `if`.

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
  an instruction comparing them. An `int` and a `float` compare by value in Starlark; known only
  at run time, they are not compared, since converting the `int` to a `float` could round it and
  change the answer.
- `None` does not exist at run time (2.5). A function that returns a value must return one on
  every path, and a function that returns none cannot be used as a value
  (`STAR_UNSUPPORTED_SYNTAX`).
- Expressions whose operands are all constants are folded at ingestion with Starlark's
  semantics, so they need no instruction. A constant may hold at most 2^20 values and bytes,
  counting each value, each byte of its strings, bytes and dict keys, and a value a list holds
  twice twice, and its lists and dicts may nest at most 64 deep, counting nesting through other
  constants; a larger or deeper one is `STAR_NOT_LOWERABLE`. All the strings, bytes, lists and
  dicts a procedure builds together may hold at most 2^24 values and bytes. Searches in
  constants take time linear in their sizes, and all the folds of a procedure together may read
  at most 2^30 values and bytes (each fold counts the sizes of the constants it reads). A dict literal that repeats a key is
  rejected, as Starlark rejects it, and an `int` and a `float` constant compare by their exact
  values, without rounding the `int` to a `float`.
- An index into bytes known only at run time must not be negative: without a length
  instruction the transpiler cannot count it from the end, so a negative index stops the
  procedure with an index error where Starlark would count from the end. A constant negative
  index is `STAR_NOT_LOWERABLE`.

---

### 4.9 Flash plans and preconditions

**Lowering (ADR-336).** The converter implements `# @preconditions`, the copy of `identity.csv` into every program (below), the flash plan's annotations and shape rules (the syntax of `@flashPlan`, `@erase` and `@recoveryRequired` and their placement, the shape rules and the mandatory preconditions, per file; the session, the `@erase` against the session's steps, the recovery facts of the session's row, the identity rows of a restartable plan and the UDS layer, per variant), and the lowering of a plan: the body is expanded in place at its call in `main`, every function it calls is expanded in place at its call (Coverage below), `diag.flash_transfer` expands as 3.2 says, the `CheckState` default of 4.6 applies inside it, and the plan gets its `FlashRecovery`. Not lowered yet: a plan function that takes parameters (`STAR_NOT_LOWERABLE` at the call).

Where each check runs. The per-file checks run on the syntax alone, whatever the variants declare, and are reported once. They are: the annotation placement (`STAR_BAD_ANNOTATION`) and ranges (`STAR_ARGUMENT_OUT_OF_RANGE`, which also refuses the reserved maximum session id); every rule of the shape list (`STAR_PLAN_SHAPE`); and the mandatory preconditions. The per-variant checks need the variant's tables and run in the lowering, before any instruction is emitted: `STAR_UNKNOWN_FLASH_SESSION`, the `erase-memory` cross-check of `@erase` and the identity rows (`STAR_PLAN_SHAPE`), and `STAR_NOT_IN_PROFILE`. A `diag.flash_transfer` whose session is an integer literal (with a `+` or `-` sign, of any size), or a top-level constant bound to such a literal or to an earlier constant that is, and not hidden by a parameter or local of the plan, is compared with the annotation's session by the per-file check; any other expression is evaluated by the lowering of the call, which refuses a session other than the annotation's as `STAR_PLAN_SHAPE` at the argument (and a value known only at run time as `STAR_NON_CONSTANT_ARGUMENT`). The lowering also refuses a `diag.flash_transfer` outside a plan function as `STAR_PLAN_SHAPE` (one in a function nothing reaches from `main`, which the lowering never sees, is refused by the per-file check with the same code), and a plan whose session's row in `flash_sessions.csv` lacks `session_timeout_ms`, `teardown_margin_ms`, `ecu_startup_ms`, `confirmation_window_ms` or `version_read_retries` as `STAR_PLAN_SHAPE` at the annotation (`csv-tables.md` 4.12).

A flash recovery plan (design 8.2.5) is a whole top-level function that the lowering expands in
place at its one call site in `main`. ECU facts (identity and precondition sources, the plan's
timings and block format) come from the definition directory's tables (`csv-tables.md` 4.12, 4.13
and 4.14); the annotations below give only what the procedure knows.

```python
# @section uninterruptible expected=180000
# @flashPlan session=1 maxResumes=3 recovery=never
def write_application():
    diag.request(0x10, {"sub": 0x02})
    diag.security_access(0x11)
    # @erase
    diag.routine(0xFF00, 0x01, {})
    res = diag.flash_transfer(1)
    if not res["ok"]:
        diag.fail("TRANSFER_EXIT_REFUSED", "")
    diag.routine(0xFF01, 0x01, {})
    diag.request(0x11, {"reset": 0x01})

# @preconditions voltage externalSupply ignition engine vehicleSpeed
def main():
    write_application()
```

The example write procedure, `examples/sequence.example.star`, is a plan of this shape for session 1
of `examples/example-ecu/`, called once from `main` with `# @preconditions voltage externalSupply
ignition`: it enters the programming session, unlocks, erases (`# @erase`), transfers, checks with
`checkProgrammingDependencies` and resets the ECU, calls no function (a plan may, as Coverage below says), and keeps the default
recovery point at the erase, so it needs no identity rows. The golden procedure
(`tests/golden/procedure.star`) holds such a plan too, so the golden files record its expansion
and `Program.flash`.

- **`# @flashPlan session=<int> [maxResumes=<int>] [recovery=never]`.** It stands directly before a
  top-level `def` other than `main`, under the placement rules of `@section` (4.5), and may share
  the `def` with one. `session` must name a `FlashSession` of the target the call
  addresses (`STAR_UNKNOWN_FLASH_SESSION`). `maxResumes` is an integer from 1 to 65535
  (`STAR_ARGUMENT_OUT_OF_RANGE`) and defaults to 3. The plan's stage is the session id; the
  maximum id is refused because it is the reserved id of a plain procedure. `@flashPlan`,
  `@erase` and `@recoveryRequired` follow the placement rules of 4.5 for a `def` and of 4.6 for a
  statement: alone at the start of a line, directly before what they annotate, with only comments
  and blank lines between; a misplaced one is `STAR_BAD_ANNOTATION`.
- **Shape (`STAR_PLAN_SHAPE`).** Each is an error: the function is not called exactly once; the
  call is not a statement of `main`'s body (inside an `if` is allowed, inside a `for` is not);
  the call is in another function, a plan or a section function; the plan has a `return` (it ends
  by falling off its body or by `diag.fail`); the plan is the callee of a section function; a plan
  function is never called; `main` carries the annotation (nothing calls it, so it has no call
  site to expand at); the plan function carries no `@section uninterruptible` or
  `@section recoveryRequired` (4.5); the plan's `diag.flash_transfer` call names another session than its
  annotation, or the plan has no `diag.flash_transfer` call or more than one; a `diag.flash_transfer` in a function the plan calls, directly or not, section functions included, is refused at that call, since the expansion would make a second download.
- **Coverage.** Everything that runs during the call is in the plan. Functions it calls, directly
  or not, section functions included, are expanded in place, not copied and called: a plan
  contains no `Call` or `Ret` (ADR-245 item 4). The plan has no frame of its own: its locals and
  the transfer's loop counter take fresh slots in `main`'s frame, and its section covers the whole
  expansion, each primitive in a piece of its own with its idempotency (4.5, ADR-332). At a call
  the arguments are evaluated and left on the stack, the callee's body stores them into its
  parameters, a `return` becomes a jump to the join after the body with its value on the stack as
  a call leaves it (so a `return` inside a loop is a jump out of it), and the callee's parameters,
  locals and temporaries take fresh slots after those the caller has used. A function called twice
  is expanded twice, each with slots of its own. A section function's expansion has its section's
  pieces laid over its range (4.5), entered by fallthrough at the first (the parameter stores),
  inside the plan's pieces; `Program::validate`'s rules for sections apply as to any: a
  `recoveryRequired` section function over the part of the plan before the recovery point is
  refused (`ContradictoryInterruptibility`, `STAR_NOT_LOWERABLE`, see Validation). The statement
  around a call does not reach into the expansion: its `# @idempotency` (4.6) does not apply to
  the callee's calls, a call in an annotated statement that lowers to no `diag` call there is
  `STAR_BAD_ANNOTATION`, and no `break` or `continue` crosses it. Recursion is refused by the
  checker as everywhere (4.1); a transfer, `@erase` or `@recoveryRequired` in a callee is refused
  by the shape rules above, so each boundary is a top-level statement of the plan itself. The
  expansions count toward the specialization budget of 2.6 in copies and instructions, each
  expansion's own instructions once however deep the chain of calls, and the limit is checked
  after each expansion, at its call (`STAR_SPECIALIZATION_LIMIT`).
  An expansion has slots of its own but no fresh frame, so one that runs more than once would
  start with the locals of the pass before, where a call starts with none (reading a local
  before assigning it is `VmError::UndefinedVariable`). A call inside a run-time loop (a `range`
  loop of the plan, or of a function the plan calls that is itself expanded) is therefore
  `STAR_NOT_LOWERABLE` when the callee, or a function it expands in turn, has a local that a
  path may read before it assigns it. The check is conservative: parameters are assigned; an `if`
  keeps what both branches assign; a loop body assigns nothing for the code after the loop, and
  its variable only inside it; a `return`, `break`, `continue` or `diag.fail` ends a path, which
  does not count against the join of an `if`. An augmented assignment reads its target. The one
  loop whose body assigns for the code after it is a `range` loop whose bounds and step are
  constants known at ingestion (literals, top-level constants, or folds of them), that yields at
  least one value, and whose body has no `break`: the names every path to the end of an
  iteration assigns, a `continue` path included, are assigned after it, its variable too. A
  bound known only at run time keeps the rule above.
  A call outside every run-time loop, and one in a loop over a constant list or dict (each pass
  of which is expanded again), is not refused.
- **Boundaries.** Derived from the lowered code. The entry is the first instruction of the
  expansion and the post-transfer end the pc after its last. The transfer exit is the RequestTransferExit the `diag.flash_transfer`
  expansion emits. The erase point is the `diag.routine` statement annotated `# @erase`, a single
  call and a top-level statement of the plan before the transfer; with no `erase-memory` step in
  the session `@erase` is refused and the erase point is the RequestDownload, and with one it is
  required. Each of those two statements is the one call, or a name assigned from it, with no other `diag` call inside it and no call to a function of the program, which the expansion would inline into it; built-ins and methods of values are allowed (`STAR_PLAN_SHAPE`), so each boundary is one operation. The `@erase` and `diag.flash_transfer` statements are top-level statements of the plan
  so no jump skips them; loops and `if`s between them are fine.
- **Recovery-required point.** By default the plan requires on-site intervention from the erase
  point (no automatic restart after it). `recovery=never` declares that no step does.
  `# @recoveryRequired` before a top-level statement of the plan, like `@erase` and the transfer (a nested statement is `STAR_BAD_ANNOTATION`, and so is a statement that emits no instruction, such as a docstring or `pass`, found when the plan is lowered), moves the point to that statement's first
  instruction, so the point lies on the plan's straight-line path and no loop jumps back across it; it is exclusive with `recovery=never`. A plan function whose section is
  `recoveryRequired` requires on-site intervention from its entry, the only point that agrees with
  a section covering the whole plan (`Program::validate` refuses a contradiction); `# @recoveryRequired`
  and `recovery=never` are refused in it (`STAR_PLAN_SHAPE`).
- **`# @preconditions <name> ...`** stands directly before `def main()`, alone at the start of its
  line, with only comments and blank lines (and a `@section`) between, and names preconditions
  among `voltage`, `externalSupply`, `ignition`, `engine` and `vehicleSpeed`. A name outside
  those (the table's `external_supply` is not one), a name given twice, none, a second
  annotation, or one that is not directly before `main` is `STAR_BAD_ANNOTATION`. Each name needs
  a row in the variant's `preconditions.csv` (`STAR_UNKNOWN_PRECONDITION`, one error at the
  annotation naming the missing kinds in the table's spelling). The program carries exactly the
  named preconditions, each as `Precondition { satisfied, default_session, programming_session }`
  from its row (`csv-tables.md` 4.14); without the annotation it declares none. The annotation is not what `diag.precondition(name)` reads from (3.2): that call reads a value while the program runs, from its own row's default-session source, whether or not the program names the kind here. A service-backed call must therefore run while the ECU is in a session that answers that read, normally the default session; a negative or undecodable answer ends the run with a `VmError` (terminal), and an input the agent cannot establish is a host error the job can be resumed after (ADR-338). Every program of
  the variant carries `identity.csv` as `Program.identity`, whether or not it names any
  precondition. A procedure with a flash plan must carry the annotation and name at least
  `voltage`, `externalSupply` and `ignition`, the checks design 8.9.1 makes mandatory before
  reprogramming (`STAR_PLAN_SHAPE`).
- **Targets.** A plan's target is the target of the handle `diag.flash_transfer` is called on.
  Its identity sources come from that target's `identity.csv`, the VIN source from the first
  access target's table when the procedure has access targets and else from the primary's, and
  the precondition sources from the reprogramming target's table (the primary's for a plain
  procedure).
- **Identity for a restartable plan.** A plan that allows a restart (`recovery=never`, or a
  recovery-required point after the erase) needs all three rows of the variant's `identity.csv`
  (`vin`, `hardware_part_number`, `software_version`), which the restart reads
  (`STAR_PLAN_SHAPE`, naming the missing kind).
- **UDS only, one segment.** `diag.flash_transfer` or `@flashPlan` in a variant whose application
  layer is not UDS is `STAR_NOT_IN_PROFILE`. A session has one segment per plan.
- **Recovery facts.** The plan's `FlashRecovery` has the session id as `flash_session` and stage,
  `maxResumes` from `@flashPlan`, the recovery point and boundaries above, and, from the session's
  row in `flash_sessions.csv` (`csv-tables.md` 4.12), `RecoveryTiming`, `version_read_retries` and
  `no_application`. A plan needs the five columns that are required "with a plan" there
  (`STAR_PLAN_SHAPE` at the annotation otherwise). The identity and precondition sources are the
  program's (above).
- **Validation.** `Program::validate` runs on every lowered program. A refusal that names a pc is
  `STAR_PLAN_INVALID` at the source of that pc; one without a pc stays `STAR_NOT_LOWERABLE`, at the
  first statement of the file, except that one naming a section (an unsafe section in the replay
  of a restartable plan, a `recoveryRequired` section before the recovery point) is at the call
  that expanded the section function, or at the section's first instruction when it is no
  expansion.
- **Layout.** `main`'s body is emitted at pc 0 and ends in a `Ret`, so no `Call` leads to a plan
  (ADR-336 item 5, amending ADR-304 item 1).

## 5. Error Reporting

**All subset violations are listed**. Do not stop at the first one.

A syntax error is different: the parser (6) stops at the first one and returns no AST, so a file
that does not parse reports that single error (`STAR_SYNTAX_ERROR`, or
`STAR_UNBOUNDED_NOT_ALLOWED` when the parser stops at `while`, which it reads as a reserved word).
A file past a limit of 2.8 is not parsed at all and reports that single error
(`STAR_SOURCE_TOO_LARGE` or `STAR_NESTING_TOO_DEEP`).
Subset validation runs on a file that parses and lists every violation it finds, sorted by
position. Lines and columns count from 1; the column counts characters. The snippet is the
source line; of a line longer than 160 characters it is the 160 characters around the column, so
that many violations on one long line stay small.
A file that passes is lowered once for each variant of its definition directory (ADR-311). Each
distinct error or warning of those lowerings is listed once; one that only some variants give
names them at the end of its hint (`(in variant V1)`, `(in variants V1, V2)`).

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

The error codes of the restart declaration (ADR-336, 4.9); the converter reports each of them, `STAR_UNKNOWN_FLASH_SESSION` and `STAR_PLAN_SHAPE` from its checks of the flash plan and its lowering (4.9), `STAR_PLAN_INVALID` from the validation of a lowered program:

| Code | Meaning |
|---|---|
| `STAR_PLAN_SHAPE` | a `@flashPlan` function breaks a shape rule (call count, call site, `return`, section callee, never called, section, `diag.flash_transfer`, `@erase`, `@recoveryRequired`), the variant's `erase-memory` step disagrees with `@erase`, a restartable plan lacks identity rows, the plan's session lacks the recovery facts of its `flash_sessions.csv` row, a `diag.flash_transfer` is outside a plan function or names another session than the annotation (found when the call is lowered), or the procedure's `# @preconditions` lacks `voltage`, `externalSupply` or `ignition` |
| `STAR_UNKNOWN_FLASH_SESSION` | the `session` of `# @flashPlan` is not a flash session of the variant (reported at the annotation) |
| `STAR_PLAN_INVALID` | the lowered program is refused by `Program::validate` with an error that names a pc; located at the source of that pc (an error without a pc is `STAR_NOT_LOWERABLE`) |
| `STAR_UNKNOWN_IDENTITY` | a key of `diag.ecu_info()` (`vin`, `part_number`, `software_version`) has no row in the variant's `identity.csv` (reported at the key; ADR-339) |
| `STAR_UNKNOWN_PRECONDITION` | a name in `# @preconditions`, or the kind a `diag.precondition` call names, has no row in the variant's `preconditions.csv` (for the call, also a row with no default-session source, which the tables never produce) |

The lowering also reports this code (3.1):

| Code | Meaning |
|---|---|
| `STAR_SECRET_IN_TEXT` | a `diag.log` message or `diag.fail` detail is built from a value read from a SecurityAccess or Authentication response: a `diag.security_access` result, or a `diag.request` of SID 0x27 or 0x29 (reported at the message or detail; ADR-348 item 6) |

The conversion reports this code after lowering (6, ADR-391):

| Code | Meaning |
|---|---|
| `STAR_DRY_RUN_STOPPED` | the dry run of the lowered procedure on answers built from the variant's declarations stopped for a reason other than the procedure's own end: an instruction failed (an index outside a list, a division by zero, a response that was not built from a declaration and does not decode, and the like), the host could not answer, or the run did not end within the step budget. Located at the source of the instruction the run stopped on; the hint gives the reason, or `did not end within N instructions`. A run that finishes, one that ends in `diag.fail` or in the lowering's own failure (such as a loop past its limit), and one that stops on a built answer that does not decode as its service's declaration are accepted |

---

## 6. Implementation Notes

- Parse with the syntax crate of `starlark-rust` (`starlark_syntax`), using only its parser and
  AST, not its evaluator. Walk the AST to detect 2.3
- The limits of 2.8 are checked by one pass over the tokens of `starlark_syntax`'s own lexer,
  before the parser runs: its parser, the validation it runs after parsing, the checker's walk
  and the AST's drop all recurse once per level of the AST, and every inner node of an
  expression has a token of its own, so the counts bound that depth. The pass, the parse and the
  check run, and the lowering walks the AST, on a thread with a fixed 16 MiB stack, about three
  times what a file at every limit at once needs in a debug build; `starlark_syntax` is built
  with optimisation in the dev profile too, since its unoptimised lexer recurses per character of
  a long run of digits or spaces (ADR-303)
- Detection uses an "allowlist approach". Unknown node kinds are rejected as
  `STAR_UNSUPPORTED_SYNTAX` so that nothing slips through by oversight
- Comment annotations (4.2, 4.5, 4.6, and 4.9) are taken from the comment tokens the parser's lexer
  reports, never from raw source lines, so `# @` inside a string literal is not an annotation.
  The text after the one comment marker must start with `@`, so `## @...` is an ordinary comment.
  Each annotation is attached by its line to the following statement. An annotation belongs to
  a top-level `def` when the line of its comment is indented further than `def` and comes before the next
  top-level statement, so a comment after the last statement of a body still belongs to it.
  Within it, an annotation belongs to a nested `def` (2.6) whose body holds the line in the same
  way. A comment after a statement takes that statement's line and indentation.
  `@maxIterations` takes an integer from 1 to 2^32 - 1 and must precede a `for` in the same
  function; `@idempotency` takes `safe`, `checkState` or `unsafe`, must start its own line and precede a
  statement in the same function; `@section` belongs to no function, starts its line, and must have a
  top-level `def` as the next line of code (4.5); `@preconditions` the same with `def main()` (4.9).
  An integer, negative ones included, outside its range is `STAR_ARGUMENT_OUT_OF_RANGE`, any
  other fault `STAR_BAD_ANNOTATION`
- Section nesting (4.5) is found on the call graph the recursion check builds, in one pass: the
  functions reachable from any section function that runs outside every plan's expansion are
  marked (without entering a plan), and a call from a marked function
  to a section function is reported, unless it closes a cycle, which is reported as recursion
  only. Calls through function parameters (2.6) are followed along the specialized call paths
- Sections (4.5) are lowered as copies keyed by the function and the section function whose call
  they run in, or none outside every section: starting from `main`, a call to
  a section function reaches that function's section copy, and any other call stays in the
  caller's section. Every section copy counts toward the budget of 2.6, in copies and in
  instructions, before it is made
- A flash plan (4.9) is not a copy: its call in `main` is replaced by the plan's body, lowered
  with the plan's own names and then moved into `main`'s frame (every local slot the plan used,
  loop counter included, is shifted past the ones `main` has used so far). Zero-width markers in
  the code give the assembly the plan's entry and end and the pcs of the erase, the recovery point
  and the transfer exit, so the boundaries come from the laid-out code; the plan function's
  section is laid over the range, cut around each primitive as a section copy is
- Idempotency (4.6) is a tag on each diagnostic primitive instruction, set where the `diag` call
  is lowered: the value of the innermost `@idempotency` statement being lowered, else the default
  of the Starlark call. The tag stays with the instruction in the function's code, so each copy
  carries it. The annotation is attached to its statement by the byte offset where that statement
  begins, which tells apart the statements that share a line, and is in force while that statement
  is lowered, nested statements included, but not while a called function is. Assembly cuts each
  section copy's range around its primitives: a primitive gets a `Section` over its instruction
  alone, the pieces between get `CheckState`, and all take the section function's interruptibility
  and expected duration (`Yes` and 0 outside every section) (ADR-332). A statement is lowered
  also when its branch can never run, and that counts as lowering its primitives. An annotated
  statement that lowers to none is `STAR_BAD_ANNOTATION`. An annotation counts as used when its
  statement lowers a `diag` call that reaches a primitive, even in a branch or operand the
  lowering then folds away, and even when an inner annotation takes precedence over it
  everywhere
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
- After generation, verify with a dry run on the `diag-ir` VM (vehicle access mocked) (12.1): `diag_ir::dry_run` with a script of answers, or with answers built from the declaration part (ADR-328). The dry run also answers the profile's seed and send-key requests of `diag.security_access`, which no declared service answers: it finds them by their place around the `SecurityAccess` step and answers a seed request with a scripted or a built seed and the send-key request with a built positive response, so a variant that declares no SecurityAccess service still runs (ADR-390); a scripted seed of zero bytes takes the unlocked path, with no `SecurityAccess` event and no send-key request (ADR-405)
- The conversion runs this dry run on every lowered procedure for each variant (ADR-391), with `Script::built_defaults()` (a key of four zero bytes, the empty positive answer to every form and record template, and 0 for every runtime input, so a procedure that uses `diag.security_access`, `diag.hmi`, `diag.record` or a precondition runs) and `diag_ir::DEFAULT_MAX_STEPS` instructions, the agent's default step budget. One function decides the end: `Finished`, any `Failed` and an `Undecodable` response of a built answer are accepted, and everything else (another VM error, a host error, the step limit, a wait, an invalid program) is `STAR_DRY_RUN_STOPPED` at `DryRunReport::pc`, merged across variants as in ADR-311. Built answers are zeros, so a procedure must also guard a value that is out of range when the vehicle's data is zero (an index into a list that is empty, a negative offset)

## 7. Verification

**Differential tests**: Import the same diagnostic procedure via ODX/OTX and via Starlark, and
confirm that the generated IR instruction sequences match. This directly verifies
replaceability (8.2).

**Golden tests**: Keep pairs of Starlark source and expected bytecode to detect regressions. `tests/golden.rs` lowers `tests/golden/procedure.star` for each variant of the example ECU and compares the program, as part of the IR, and the requests it sends with the expected files in `tests/golden/`; run it with `NGR_UPDATE_GOLDEN=1` to record an intended change, and review the diff.
