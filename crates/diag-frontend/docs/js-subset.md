# JavaScript Subset Specification (JS -> IR)

Corresponds to design document 8.4. Defines the accepted syntax and conversion rules
for converting the custom format (CSV tables + JavaScript) into IR.

**Premise**: No JavaScript engine is shipped on the agent. Conversion is performed server-side by
`diag-frontend`, and the output is `diag-ir` bytecode.
Runtime behavior is exactly the same as IR derived from ODX/OTX.

---

## 1. Design Principles

| Principle | Rationale |
|---|---|
| Reject anything that cannot be resolved statically | The VM has no instructions for dynamic resolution at runtime |
| Guarantee that execution is finite | So execution never stalls inside an uninterruptible section (8.10.1) |
| Preserve determinism | So execution can be reproduced from the audit log (R156) |
| Report all violations at import time | Avoid discovering unsupported constructs at runtime |

---

## 2. Supported Syntax

### 2.1 Accepted

| Category | Syntax |
|---|---|
| Declarations | `let`, `const` |
| Operators | Arithmetic, comparison, logical (`&&` `\|\|` `!`), bitwise, ternary operator |
| Control flow | `if` / `else`, `for`, `while`, `do-while`, `switch`, `break`, `continue` |
| Functions | Top-level `function` declarations, calls to them, `return` |
| Literals | Numbers, strings, booleans, arrays, objects (all static) |
| Indexing | Array index access, object property access (fixed keys) |
| Built-ins | The restricted list in 2.3 |
| API | The diagnostic primitives in section 3 |

### 2.2 Rejected (error at import time)

| Syntax | Rationale | Error code |
|---|---|---|
| `var` | Hoisting behavior is counterintuitive | `JS_VAR_NOT_ALLOWED` |
| Arrow functions, function expressions, closures | The VM has no environment capture | `JS_CLOSURE_NOT_ALLOWED` |
| Higher-order functions (passing functions as values) | Callee cannot be resolved statically | `JS_FUNCTION_VALUE_NOT_ALLOWED` |
| `eval`, `new Function`, dynamic `import` | Dynamic code generation | `JS_DYNAMIC_CODE_NOT_ALLOWED` |
| `class`, `new`, prototype manipulation, `this` | No object model | `JS_OOP_NOT_ALLOWED` |
| `async` / `await`, Promise, generators | Replaced by the synchronous model in 3.2 | `JS_ASYNC_NOT_ALLOWED` |
| `try` / `catch` / `throw` | Replaced by the approach in 4.3 | `JS_EXCEPTION_NOT_ALLOWED` |
| Regular expression literals | Execution time cannot be estimated statically | `JS_REGEXP_NOT_ALLOWED` |
| Destructuring, spread, default parameters | Not supported in version 1 (may be added later) | `JS_UNSUPPORTED_SYNTAX` |
| Dynamic property access `obj[expr]` | Key cannot be resolved statically (array indexing is allowed) | `JS_DYNAMIC_PROPERTY_NOT_ALLOWED` |
| Modules (`import` / `export`) | One file = one procedure | `JS_MODULE_NOT_ALLOWED` |
| `Date`, `Math.random`, `globalThis` | Breaks determinism | `JS_NONDETERMINISTIC` |

### 2.3 Supported Built-ins

Numbers: `Math.abs` `Math.min` `Math.max` `Math.floor` `Math.ceil` `Math.round`
`Math.pow` `Math.sqrt` `Number.parseInt` `Number.parseFloat` `Number.isNaN`

Strings: `length` `slice` `indexOf` `startsWith` `endsWith` `toUpperCase`
`toLowerCase` `padStart` `trim`

Arrays: `length` `push` `pop` `slice` `indexOf` `includes` `join`

Byte arrays: `bytes.get(i)` `bytes.length` `bytes.slice(a, b)` `bytes.toHex()`
`bytes.fromHex(s)` `bytes.readUint(offset, bits)` `bytes.writeUint(offset, bits, v)`

`map` / `filter` / `reduce` are **not supported** because they take functions as values. Use `for` instead.

---

## 3. Diagnostic Primitive API

### 3.1 Overview

Provided as methods of the `diag` object. Each method maps 1:1 to a `diag-ir`
instruction (8.2.4).

```js
diag.request(serviceId, params)        // Execute service -> response object
diag.readDtc(mask)                     // Read DTCs
diag.routine(routineId, sub, params)   // Routine control
diag.securityAccess(level)             // Send seed -> receive key (online required)
diag.flashTransfer(sessionId)          // Flash transfer (carries section attributes)
diag.wait(millis)                      // Wait
diag.hmi(formId, params)               // HMI request -> input value
diag.record(templateId)                // Request input for a record template
diag.capture(backMillis)               // Capture a monitoring window
diag.log(level, message)               // Log output
diag.ecuInfo()                         // Variant, part number, SW version (read-only)
diag.precondition(name)                // Current value of an execution precondition (voltage, vehicle speed, etc.)
```

### 3.2 Write as Synchronous Calls

Internally these involve waiting, but `async` / `await` is not used.
The VM maps them to wait instructions, so scripts can be written synchronously.

```js
const res = diag.request(0x22, { did: 0xF190 });  // Read VIN
diag.log(1, res.fields.vin);
```

### 3.3 Response Shape

```ts
{
  ok: boolean,
  nrc: number | null,      // Negative response code (NRC) (when ok=false)
  fields: { [name: string]: number | string | Bytes },
  raw: Bytes
}
```

The keys of `fields` are `Field.short_name` from the IR declaration part. They are defined on the CSV table side.

---

## 4. Conversion Rules

### 4.1 Static Resolution

- Function calls must resolve to a top-level declaration in the same file
- Recursion is forbidden (`JS_RECURSION_NOT_ALLOWED`). Detected by building a call graph
- Object properties use fixed keys only. `obj.a` is allowed; `obj[k]` is not

### 4.2 Loop Limits

Loops whose iteration count is not statically determined get a runtime limit embedded.

```js
// @maxIterations 100
while (!done) { ... }
```

Without the annotation, the default (1000) is applied and reported as a warning.
When the limit is reached, the VM stops with `LoopLimitExceeded`.

### 4.3 Error Handling

Use return values instead of exceptions.

```js
const res = diag.request(0x22, { did: 0xF190 });
if (!res.ok) {
  diag.log(3, "Read failed NRC=" + res.nrc);
  return;                       // Abort the procedure
}
```

`return` means normal termination of the procedure. For abnormal termination, use `diag.fail(code, detail)`.

### 4.4 Specifying Section Attributes

Interruptibility (8.10.1) is specified with annotations. Ranges without a specification are `interruptible: yes`.

```js
// @section uninterruptible expected=120000
diag.flashTransfer(1);
// @endsection
```

| Annotation | `Interruptible` |
|---|---|
| `@section interruptible` | `Yes` |
| `@section uninterruptible` | `No` |
| `@section recoveryRequired` | `RecoveryRequired` |

`expected=<millis>` is the expected duration. It is compared with the remaining OEM authentication time (8.10.1).

### 4.5 Specifying Idempotency

Behavior on resume (8.2.5) is specified with annotations. When omitted, the per-API default is used
(`Safe` for reads; `CheckState` for `routine` and `flashTransfer`).

```js
// @idempotency unsafe
diag.routine(0xFF00, 0x01, {});
```

### 4.6 Source Maps

The source file line and column are recorded for each instruction and stored in `Program.source_map`.
This allows execution traces and audit logs to refer back to the original source.

---

## 5. Error Reporting

**All violations are listed**. Do not stop at the first one.

```json
{
  "status": "rejected",
  "errors": [
    {
      "code": "JS_CLOSURE_NOT_ALLOWED",
      "file": "sequence.js",
      "line": 42,
      "column": 15,
      "snippet": "const f = (x) => x + 1;",
      "hint": "Rewrite as a top-level function declaration"
    }
  ],
  "warnings": [
    {
      "code": "JS_LOOP_LIMIT_DEFAULTED",
      "file": "sequence.js",
      "line": 88,
      "hint": "Specify @maxIterations (default 1000 applied)"
    }
  ]
}
```

---

## 6. Implementation Notes

- Parse with `swc_ecma_parser` or `oxc_parser`. Walk the AST to detect 2.2
- Detection uses an "allowlist approach". Unknown node kinds are rejected as `JS_UNSUPPORTED_SYNTAX`
  so that nothing slips through by oversight
- Constant folding is done during bytecode generation
- After generation, verify with a dry run on the `diag-ir` VM (vehicle access mocked) (12.1)

## 7. Verification

**Differential tests**: Import the same diagnostic procedure via ODX/OTX and via JS, and confirm that
the generated IR instruction sequences match. This directly verifies replaceability (8.2).

**Golden tests**: Keep pairs of JS source and expected bytecode to detect regressions.
