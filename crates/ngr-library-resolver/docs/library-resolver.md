# Library resolver

`ngr-library-resolver` turns a VCI name into the vendor library to load. It is the shared
implementation of ADR-228 (Decision item 4); the Linux definition format is ADR-266. Design
references: 7.1.1 (Linux registration definition) and 7.2 (library trust).

## Scope

- In scope: reading Linux definition files, validating them, matching a VCI name, and on Windows
  looking the name up in the registry (through `j2534-0404-registry`).
- Not in scope yet: the writability and signer checks of 7.2, and the callers (`j2534-0404-service`,
  `agent`, `vci-discovery`) still use their own lookups.

## Definition files (non-Windows)

The directory is `vci_service_config::j2534_definition_dir()` (`/etc/nekoguruma/j2534/` in release
builds, 7.1.1). One TOML file per VCI. Only entries directly in the directory whose name ends in
`.toml` (exact, lower case) and that are regular files are read; everything else is ignored. The
file name carries no meaning.

Keys mirror the Windows registry value names. Unknown keys make the definition invalid.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `Name` | string, non-empty | yes | VCI name a caller resolves; matched exactly, case-sensitively |
| `FunctionLibrary` | string | yes | absolute path of the vendor library |
| `Vendor` | string | no | display only |
| `ConfigApplication` | string | no | display only |
| `J1850VPW`, `J1850PWM`, `ISO9141`, `ISO14230`, `CAN`, `ISO15765`, `SCI_A_ENGINE`, `SCI_A_TRANS`, `SCI_B_ENGINE`, `SCI_B_TRANS` | integer 0 or 1 | no | protocol supported (1) or not (0 or absent) |
| `LongSize` | integer 4 or 8 | no | width of `unsigned long` the library was built for (7.1.2) |
| `SearchPaths` | array of absolute path strings | no | extra search paths for dependent libraries |

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
- An invalid file (relative `FunctionLibrary` or search path, empty `Name`, protocol value other
  than 0 or 1, `LongSize` other than 4 or 8, unknown or mistyped key, unreadable file) is skipped
  and logged at warn level with its path and reason. `read_definitions` returns the invalid
  files with their `DefinitionError` for callers that want to report them.
- A missing directory counts as empty (`NotFound` on resolve); a directory that exists but cannot
  be listed is `ResolveError::Io`.

## Windows

`resolve(name)` looks the name up in the native registry view; `resolve_on_registry(name, mode)`
takes the view. A hit carries the name and library path only (protocols, `LongSize` and search
paths are empty), with `Source::Registry`. The registry value is not checked for being absolute.
