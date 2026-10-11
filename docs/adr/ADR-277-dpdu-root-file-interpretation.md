# ADR-277: D-PDU API Root Description File Interpretation

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** `ngr-library-resolver` (`iso22900` module, `RegistryView`, `docs/library-resolver.md`), design 7.1 / 7.2, ADR-228 item 4, ADR-270 item 5

## Context

ISO 22900-2:2022 clause 8.7 and Annex F define the root description file that lists the installed
D-PDU API implementations, and the schema of its entries. They do not say how a client treats a
file that violates the schema, a `file:` URI that names a host, or which registry view holds the
file's location. Design 7.1 and 7.2 ask for an explicit registry view, expansion of
`REG_EXPAND_SZ` values by the view's bitness, and a library chain that regular users cannot change. ADR-266 settles only
the Linux J2534 registration format, so the D-PDU API side had no recorded rules, and the first
implementation took lenient readings (entries found at any depth, only the first of a repeated
child, any registry value expanded, any declared encoding, UNC paths on Windows).

## Decision

1. **Identity and entries.** An implementation is identified by its `SHORT_NAME`, trimmed and
   matched exactly. The text is the element's text and CDATA children joined, so a comment inside
   does not cut it; an element inside makes the entry unusable. A name that occurs more than once
   is ambiguous, whether or not the entries are usable. Entries are the `MVCI_PDU_API` children
   of the document element `MVCI_PDU_API_ROOT` (matched by local name, so a default namespace is
   harmless); any other document element is an error, and nested entries are not entries. The
   version attribute is not checked. Of the six known children (`SHORT_NAME`, `DESCRIPTION`,
   `SUPPLIER_NAME`, `LIBRARY_FILE`, `MODULE_DESCRIPTION_FILE`, `CABLE_DESCRIPTION_FILE`) a repeated
   one makes the entry unusable, reported under its first `SHORT_NAME`; unknown children are
   ignored. Only the un-namespaced `URI` attribute counts.
2. **Only local absolute paths.** A `file:` URI is accepted when it names no host other than
   `localhost`, does not start its path with `//` (in any percent-encoding), has no `..`
   component (nor another component made only of dots and spaces, except `.`, since Windows
   drops trailing dots and spaces from a component) and no NUL after decoding, and has no raw `?` or `#` (their percent-encoded forms
   decode normally), and no empty path (`file:///`). `/\` is refused like `//`. ASCII whitespace
   around the URI is trimmed, nothing else. On Windows only the drive-letter form is accepted
   (`RelativePath` otherwise); off Windows a drive-letter path (`file:///c:/x`) is `InvalidUri`. This holds for the
   library, the MDF and the CDF on every platform, since the 7.2 premise (ADR-270) cannot be
   established for a file served over SMB. The MDF and CDF stay optional, because the library is
   found without them; the resolver warns, for the matched entry only, when the schema
   requires one the entry lacks. When present they follow the same rules and are naming files
   under ADR-270 item 5, checked together with the root file.
3. **The registry view is an input.** The lookup takes a `RegistryView` (32-bit or 64-bit);
   the worker uses its own bitness and the agent iterates the views (ADR-240). The `Root File`
   value is read raw: only `REG_EXPAND_SZ` is expanded, `REG_SZ` stays literal even with a `%`.
   Expansion never reads the process environment (design 7.2). Only a fixed list of names
   (`ProgramFiles`, `CommonProgramFiles`, their `(x86)` and `W6432` forms, `SystemRoot` and
   `windir`, case-insensitive) is expanded, from the matching HKLM string values read in the same
   registry view as `Root File`; the redirected `CurrentVersion` key is what makes the 32-bit view
   yield the x86 folders. Any other name, an empty `%%`, an unpaired `%` and a listed name without a
   value are refused (`RootFileValue`), and the result, expanded or literal, must be an absolute
   drive-letter path (no relative, UNC or `\\?\` path).
4. **Encoding.** The file is UTF-8 (a byte order mark tolerated) or UTF-16 with a little- or
   big-endian byte order mark. Anything else, UTF-32 included, is refused as an encoding error; a
   declared XML encoding is not honoured. Unmarked UTF-16 or UTF-32 is an encoding error too: its
   NUL bytes, which XML never allows, give it away.

## Consequences

- A share path, a URI with an unencoded query or fragment character, or a relative or
  parent-directory path is refused with the reason in the error, even where the vendor installer
  wrote it.
- A 64-bit agent can see 32-bit installations and name the one it wants; a 32-bit worker reads
  only its own view.
- A root file saved in a legacy code page must be saved again as UTF-8 or UTF-16.
- Residuals: a mapped network drive passes the local-path rule; under the 32-bit view the
  System32 file-system redirection can make a path name another file than a 64-bit process sees.
- `check_writability` does not yet refuse non-local paths itself, so a value that never passes
  through the URI conversion (the registry `Root File`, a J2534 `FunctionLibrary`) still needs
  that finding.

## Alternatives considered

- Accepting UNC paths on Windows only: the writability premise cannot be established there.
- Requiring the MDF and CDF: the library does not depend on them, and many installations omit
  them.
- Per-process `ExpandEnvironmentStringsW`: it expands for the caller's bitness, not the view's.
- Expanding from the process environment, with the view's `(x86)` / `W6432` variables: rejected
  per design 7.2, since another process could set the variables and so redirect the root file,
  which decides which library loads.
- Honouring any declared encoding: it needs a code-page library for no installed benefit.
- `All` or `Native` view variants: they hide which view answered; the caller names a view.
- Reusing the J2534 registry crate's view enum: it carries the `All` mode and ties this module
  to that crate.
