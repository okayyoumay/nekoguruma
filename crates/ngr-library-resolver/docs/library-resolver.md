# Library resolver

`ngr-library-resolver` turns a VCI name into the vendor library to load. It is the shared
implementation of ADR-228 (Decision item 4); the Linux definition format is ADR-266. Design
references: 7.1 (discovery), 7.1.1 (Linux registration definition) and 7.2 (library trust).

## Layout

The crate has one module per standard and, for J2534, per version (ADR-266 Decision item 5).
Each module holds that API's discovery chain and its types, and keeps the operating-system differences inside: the
same call resolves a name on every platform. What does not depend on the standard, such as the
7.2 checks, belongs at the crate root.

| Module | Standard | Linux | Windows |
|---|---|---|---|
| `j2534_0404` | SAE J2534-1 v04.04 | definition files (below) | registry, through `j2534-0404-registry` |

The rest of this document describes the `j2534_0404` module; the names below are in it
(`j2534_0404::resolve`, `j2534_0404::ResolveError`, ...).

## Scope

- In scope: reading Linux J2534 definition files, validating them, matching a VCI name, and on
  Windows looking the name up in the registry (through `j2534-0404-registry`).
- Not in scope yet: the writability and signer checks of 7.2, and the callers (`j2534-0404-service`,
  `agent`, `vci-discovery`) still use their own lookups.

## Definition files (non-Windows)

The directory is `vci_service_config::j2534_definition_dir()` (`/etc/nekoguruma/j2534/` in release
builds, 7.1.1). One TOML file per VCI. Only entries directly in the directory that are regular
files with the extension exactly `toml` (lower case) are read; everything else is ignored. A file
named just `.toml` has no extension and is ignored; hidden files such as `.x.toml` are read. A
`.toml` entry that is a symlink or a directory is not followed or read: it is reported as invalid
(`NotARegularFile`). A file larger than 64 KiB is invalid (`TooLarge`). The file name carries no
meaning. A definition directory that is itself a symlink (or, on Windows, a junction or another
reparse point) is refused as a whole (`ResolveError::LinkedDirectory`), so no definition comes
from outside it; whether its parent folders can be written by regular users is for the 7.2
checks.

Keys mirror the Windows registry value names. Unknown keys make the definition invalid.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `Name` | string, non-empty | yes | VCI identifier a caller resolves; matched exactly, case-sensitively. Refused, not trimmed, if blank or with leading/trailing whitespace |
| `FunctionLibrary` | string | yes | absolute path of the vendor library |
| `Vendor` | string | no | display only |
| `ConfigApplication` | string | no | display only |
| `J1850VPW`, `J1850PWM`, `ISO9141`, `ISO14230`, `CAN`, `ISO15765`, `SCI_A_ENGINE`, `SCI_A_TRANS`, `SCI_B_ENGINE`, `SCI_B_TRANS` | integer 0 or 1 | no | protocol supported (1) or not (0 or absent) |
| `LongSize` | integer 4 or 8 | no | width of `unsigned long` the library was built for (7.1.2) |
| `SearchPaths` | array of absolute path strings | no | extra search paths for dependent libraries |

`FunctionLibrary` and `SearchPaths` entries are also refused if they contain a NUL character,
have leading or trailing whitespace, or have a `..` component.

`Name` corresponds to the Windows device's registry key name, which is what
`j2534-0404-registry` matches; the Windows `Name` value stays display-only.

Example:

```toml
Name = "Acme - PassThru One"
Vendor = "Acme"
FunctionLibrary = "/opt/acme/lib/libacme_j2534.so"
ConfigApplication = "acme-config"
CAN = 1
ISO15765 = 1
LongSize = 8
SearchPaths = ["/opt/acme/lib", "/opt/acme/vendor"]
```

## Matching and errors

`resolve_in_dir(dir, name)` (and `resolve(name)` on Linux) returns the single valid definition
with that `Name`.

- No match: `ResolveError::NotFound`, which carries the number of invalid files skipped, so a
  typo in a definition is visible to the operator.
- Several valid definitions with the same `Name`: `ResolveError::Ambiguous` listing the files;
  none is picked.
- A leftover copy of a definition (for example a backup ending in `.toml`) with the same `Name`
  makes resolution ambiguous; remove or rename it.
- Results of `read_definitions` and the file list of `Ambiguous` are sorted by file path.
- An invalid file (non-regular file, over the size cap, other validation failures, relative `FunctionLibrary` or search path, empty `Name`, protocol value other
  than 0 or 1, `LongSize` other than 4 or 8, unknown or mistyped key, unreadable file) is skipped
  and logged at warn level with its path and reason. `read_definitions` returns the invalid
  files with their `DefinitionError` for callers that want to report them.
- A missing directory counts as empty (`NotFound` on resolve); a directory that exists but cannot
  be listed is `ResolveError::Io`.

## Windows

`resolve(name)` reads only the process's own registry view (`Native`, so the 64-bit view on 64-bit
Windows). A caller that needs both views, such as the agent choosing a worker build, calls
`resolve_on_registry(name, mode)` per view; mode `All` returns the first hit and does not detect a
name present in both views. A hit carries the name and library path only (protocols, `LongSize` and search
paths are empty), with `Source::Registry`. The registry value is not checked for being absolute.
