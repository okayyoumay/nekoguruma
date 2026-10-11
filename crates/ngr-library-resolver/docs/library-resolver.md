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
| `j2534_0404` | SAE J2534-1 v04.04 (registry: §9.2) | definition files (below) | registry, through `j2534-0404-registry` |
| `iso22900` | ISO 22900-2 D-PDU API (2022 edition, clause 8.7, Annex F) | root description file (see "ISO 22900") | the same, found through the registry in a named view |

Unless a section says otherwise, this document describes the `j2534_0404` module; the names in
it are that module's (`j2534_0404::resolve`, `j2534_0404::ResolveError`, ...). The `iso22900`
module has its own section below and its own `resolve` and `ResolveError`.

## Scope

- In scope: reading Linux J2534 definition files, validating them, matching a VCI name, and on
  Windows looking the name up in the registry (through `j2534-0404-registry`).
- Also in scope: resolving a D-PDU API implementation name through the root description file
  (`iso22900`).
- Also in scope: the writability check and the signer check of 7.2 (below), standard-independent
  and at the crate root, and `check_library`, which runs both.
- Not in scope yet: the callers (`j2534-0404-service`, `agent`, `iso22900-service`,
  `vci-discovery`) still use their own lookups.

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

## ISO 22900 (D-PDU API)

The `iso22900` module resolves an implementation name (design 7.1; ISO 22900-2:2022 clause 8.7 and
Annex F, cited by clause only).

**Chain.** The root description file lists one `MVCI_PDU_API` entry per installed implementation.
The entry itself names the API library (`LIBRARY_FILE`), the module description file
(`MODULE_DESCRIPTION_FILE`) and the cable description file (`CABLE_DESCRIPTION_FILE`), each as a
`URI` attribute holding a `file:` URI. The MDF and CDF are not parsed and not followed to find the
library; their paths are reported in `Implementation` for the 7.2 check. ADR-277 is the decision
record for the rules below.

**Root file location** (`root_file_path(view)`). On Windows the registry view `view` of
`HKLM\SOFTWARE\D-PDU API`, value `Root File` (a missing key, value or blank content is `None`).
Elsewhere `vci_service_config::pdu_api_root_file()` (`/etc/pdu_api_root.xml` in release builds,
fixed at build time by `NGR_PDU_API_ROOT_FILE`, ADR-228 Decision item 1; debug builds also read
the variable at run time, ADR-073); the view is ignored there. `resolve_in_view(name, view)`
locates the root file and calls `resolve_in_root_file(root_file, name)`; `resolve(name)` uses
`RegistryView::native()`. `RegistryView` (crate root) has two values, `Wow64_32` and `Wow64_64`,
and no "all views" mode: a worker reads its own bitness, and the agent calls once per view.

**Registry value.** The value is read raw. `REG_SZ` is taken literally, even if it contains `%`.
`REG_EXPAND_SZ` has its `%NAME%` references expanded, but never from the process environment: a
file that decides which library loads is not located through variables another process could set
(7.2). Only these names are expanded (case-insensitive), from `REG_SZ` values under `HKLM`, read in
the same registry view as `Root File`:

| Name | Key | Value |
|---|---|---|
| `%ProgramFiles%` | `SOFTWARE\Microsoft\Windows\CurrentVersion` | `ProgramFilesDir` |
| `%CommonProgramFiles%` | same | `CommonFilesDir` |
| `%ProgramFiles(x86)%` | same | `ProgramFilesDir (x86)` |
| `%CommonProgramFiles(x86)%` | same | `CommonFilesDir (x86)` |
| `%ProgramW6432%` | same | `ProgramW6432Dir` |
| `%CommonProgramW6432%` | same | `CommonW6432Dir` |
| `%SystemRoot%`, `%windir%` | `SOFTWARE\Microsoft\Windows NT\CurrentVersion` | `SystemRoot` |

The 32-bit view redirects `CurrentVersion`, so `ProgramFilesDir` there is the x86 folder; that is
how the view's bitness is honoured. Any other `%NAME%`, an empty `%%`, an unpaired `%`, and a
listed name whose value is missing, unreadable, not a `REG_SZ`, empty or holding a NUL are refused; expanded text is not scanned
again. After expansion (and for a literal `REG_SZ` value too) a non-blank `Root File` must be an
absolute drive-letter path (`X:\...` or `X:/...`); a relative path, a UNC path (`\\` or `//`) and a
`\\?\` path are refused, also when a folder value brings it in (a UNC folder, say). The same
parent rule as for the URI applies to the result, split on `\` and `/`: a `..` component, or any
other made only of dots and spaces except `.`, is refused. Only ASCII whitespace is trimmed from the
value (U+00A0 and U+3000 are not). All of these are `ResolveError::RootFileValue`, as are invalid
UTF-16 and a NUL, in the value or in an expanded folder. A value of another registry type is `ResolveError::Registry`.

**Encoding and size.** The file is read up to `MAX_ROOT_FILE_SIZE` (1 MiB, inclusive; more is
`TooLarge`). It must be UTF-8 (a byte order mark is tolerated) or UTF-16 little- or big-endian
with a byte order mark; anything else, including a legacy code page, UTF-32 with or without a
mark and UTF-16 without one, is `Encoding`. Unmarked UTF-16 or UTF-32 text is recognised by the NUL bytes it
contains (a NUL character never appears in XML). The XML
declaration's encoding is not honoured. `parse_root_file` takes text.

**Structure.** The document element must be `MVCI_PDU_API_ROOT` (`NotARootFile` otherwise; the
version attribute is not checked). Entries are its direct `MVCI_PDU_API` children; one nested
elsewhere is ignored. Elements are matched by local name, so a default namespace is harmless, and
only the un-namespaced `URI` attribute counts. Unknown children are ignored. A repeated
`SHORT_NAME`, `DESCRIPTION`, `SUPPLIER_NAME`, `LIBRARY_FILE`, `MODULE_DESCRIPTION_FILE` or
`CABLE_DESCRIPTION_FILE` makes the entry unusable (`Duplicate`), reported under its first
`SHORT_NAME`. `SHORT_NAME` text is the element's text and CDATA joined (a comment inside does not
cut it) and trimmed; an element inside is `NestedShortName`.

**Paths.** A `file:` URI is converted to an absolute local path, for the library, the MDF and the
CDF alike, on every platform. Percent-encoding is decoded. ASCII whitespace around the URI is
trimmed; any other character (such as U+00A0) is not. A host other than `localhost` (this
includes `file://c:/dir/x.dll`) and a path starting with `//` or `/\` in any encoding
(`file:////server/..`, `file:///%5C%5Cserver/..`) are `RemoteHost`: a share cannot meet the 7.2 premise that regular
users cannot write the chain. A raw `?` or `#`, a `..` component (or any other component made only of dots and spaces except
`.`, since Windows drops trailing dots and spaces and `.. ` would name the parent there), a NUL after decoding, an empty path
(`file:///`, `file:///.`, `file:///./`: no named component), another scheme and malformed percent-encoding (each `%` needs two hex digits; a
sign is not one) are `InvalidUri`; so is a drive-letter path off Windows (`file:///c:/x`). On Windows only the drive-letter form is
accepted (a path without one is `RelativePath`, not `InvalidUri`); raw backslashes may stay. A URI that gives a
relative path (`file:dir/x`) is `RelativePath`. A mapped network drive passes these rules.

**Matching.** The name is an entry's `SHORT_NAME`, matched exactly and case-sensitively against
the name as given. The MDF and CDF are optional, but the schema (Annex F) requires them, so
resolving an entry that lacks one logs a warning for that entry.

**Errors.**

- `NoRootFile`: no registry value, or the root file does not exist. Distinct from `NotFound`.
- `NotFound`: no entry has the name; it carries the number of unusable entries skipped, so a
  typo in an entry is visible. Each skipped entry is logged at warn level.
- `Ambiguous`: more than one entry has the name (an unusable one counts); none is picked.
- `InvalidEntry`: the one entry with the name is refused, with an `EntryError`: `MissingShortName`,
  `NestedShortName`, `MissingLibrary`, `Duplicate`, `InvalidUri`, `RemoteHost` or `RelativePath`
  (the last three name the element).
- `Encoding`, `Xml` and `NotARootFile`: the file is not UTF-8 or marked UTF-16, not well-formed
  XML, or not a root description file; each names the file. `Io` is any other read failure.

`parse_root_file` and `read_root_file` return every entry: usable ones in `implementations`,
unusable ones with their reason in `invalid`, both in document order.

**Naming files.** `Resolved::naming_files()` is the root file, then the MDF and the CDF when the
entry names them. They decide which library is loaded or how it is configured, so they take part
in the 7.2 writability check; the library is passed separately:

```rust
check_writability(&r.implementation.library_file, &r.naming_files())?;
```

On Windows the registry value is not covered (HKLM is trusted by premise, 7.2), and a path that never passes through the URI conversion is not checked for being local.

## Writability check (design 7.2)

ADR-270 is the decision record. The check answers one question before a library is loaded: can a
regular user change the library, or anything that decides which file is loaded?

```rust
check_writability(library, &resolved.naming_files())?;             // Policy::system()
check_writability_with(library, &naming_files, &policy)?;           // explicit policy
```

`Resolved::naming_files()` is the definition file for `Source::Definition` and empty for
`Source::Registry` (HKLM is trusted by premise, 7.2). The result is `Err(WritabilityError)` with
every `Finding { path, role, reason }` found, one per line in its `Display`; the check does not stop
at the first. The caller decides what a finding means: device mode refuses the library, user mode
warns.

**What is checked.** The library (it must end at a regular file), its own directory, every further
ancestor up to the root, each naming file and each of its ancestors. A relative path is a finding
(`NotAbsolute`) and is not walked, and so is a path with a `..` component (`ParentComponent`). The
walk goes component by component from the root without following links silently: a symbolic link
or junction (on Windows, a name-surrogate reparse point; other reparse points such as
deduplicated files count as the file itself) is an entry of its own (`Role::Link`), then its target
is walked the same way; more than 40 links in one path is `TooManyLinks`. On Windows a link target
with a `..` is also `ParentComponent`, since Windows removes `..` by text and the walk could check
another file than the one loaded. The library-directory rule applies to the directory that holds
the final file name as reached, so a link target such as `../lib.so` cannot route around it. A walk
that ends on a root rather than a file name is `NotARegularFile`. A directory reached by several walks is reported once, with the stricter
role (`LibraryDirectory` over `Directory`). A path that cannot be inspected is an `Io` finding.

**Policy.** `Policy` is the set of owners trusted to hold write access. `Policy::system()` is uid 0
on Unix; on Windows SYSTEM, BUILTIN\Administrators and TrustedInstaller. An entry owned by anyone
else is `UnprivilegedOwner`. `trusting_current_user()` (tests and debug builds only) adds the
current user for fixtures.

**Unix rule.** Every entry, links included, must be owned by a policy uid. Other entries must have
neither the group-write nor the other-write bit. A container directory (`Role::Directory`) with the
sticky bit passes, since others can add to it but not replace what is in it; the library's own
directory gets no such exception. A link's own mode is ignored. There is no exemption for group 0:
a POSIX ACL grant shows in the group and mask bits.

**Windows rule.** Owner and DACL are read from the entry itself (opened without following a
reparse point). A null DACL is `NullDacl`. For each ACE that is not inherit-only, an allow ACE
(types 0 and 9) whose trustee is outside the policy and whose mask has a write right for the role
is `WritableByRegularUsers`; deny ACEs are ignored; the OWNER RIGHTS trustee is skipped (the owner
is checked itself); any other ACE type is treated as a finding. Write rights are data, append,
delete, change-permissions, take-ownership and the generic write, all and maximum-allowed bits. On a
link entry, write-attributes also counts.
For a container directory the file-data and append rights (which mean add-file and
add-subdirectory there) do not count, matching the sticky container on Unix; for the library's own
directory they do.

The per-entry rules are pure functions over plain data (owner, permission bits or ACE list, role,
policy) and are unit-tested on synthetic input on every platform; only the walker's calls into the
operating system differ.

## Signer check (design 7.2)

ADR-278 is the decision record. The check answers: if the library carries an embedded Authenticode
signature, does it verify against the system trust store?

```rust
let signer = check_library(library, &resolved.naming_files(), &Policy::system())?; // both checks
let signer = check_signer(library)?;                                               // signer only
```

`check_library` is the single pre-load entry point: it runs `check_writability_with` and
`check_signer` and returns `Err(PreloadError)` with every `Finding` of both (`findings`, writability
first). A file that could not be read for the signer check is `PreloadError::io`, not a finding.
The error also carries what the signer check learned: `PreloadError::signer` is the verdict when
the check ran without an I/O error and found no invalid signature (so it accompanies writability
findings), and `PreloadError::held` is the open file when the signature was invalid (there is no
`Signer` then; `SignerError::Invalid` holds the file too). User mode, which warns and loads, should
load with that verdict or file kept alive until the library is mapped; device mode drops them.
The caller decides what a finding means: device mode refuses the library, user mode warns and
loads. The check takes no mode.

**Verdict.** `Signer::NotApplicable` on every platform but Windows. On Windows:
`Signer::Unsigned` (no signature to verify) or `Signer::Trusted { subject, thumbprint_sha256, .. }`,
the signer certificate's simple display name and the SHA-256 of its encoded bytes in upper-case
hex. Both Windows variants hold the open file, see below.

**Presence.** A signature is present when the size field of the PE optional header's security data
directory (entry 4) is not zero. PE32 and PE32+ are read; a directory the header does not declare
(fewer than five entries, or an optional header too short) counts as empty. The DOS header is read
first, then the file is positioned at its `e_lfanew` and the NT headers are read there, so headers
beyond any fixed prefix are found, at any 32-bit offset as the loader accepts it; an `e_lfanew` past
the end of the file is treated as no PE image. A file that is not a PE image, or whose headers are cut off, is `Unsigned` (it has nothing to verify and fails later as a
library). Catalog signatures are not looked at, so a catalog-signed system file such as
`kernel32.dll` is `Unsigned`.

**Verification.** Only for a present signature: `WinVerifyTrust` with the generic Authenticode
action, a file choice, no user interface, no revocation check and cache-only URL retrieval, so it
makes no network request. Revocation is off both in the revocation-checks field and with the
no-revocation provider flag, so the machine-wide Software Publishing policy cannot turn it back on;
`CERT_E_REVOKED` and `CERT_E_REVOCATION_FAILURE`, should one come back anyway, are classed as
`UntrustedSigner`. The lifetime-signing flag is off: a signature with a trusted time stamp
passes after its certificate expires, one without a time stamp fails with an expiry. Only the
primary signature is looked at. The trust source is the system store; nothing is pinned.

**No protection against a writer.** Someone who can write the file can strip the signature and get
`Unsigned`, which passes. The signer check is therefore no substitute for the writability check,
which is the gate; it tells who signed a file that regular users cannot change.

**Failures.** A non-success result is `SignerError::Invalid { failure, hresult }`, and in
`check_library` one finding on the library: `Reason::InvalidSignature { failure, hresult }`,
`Display` showing the HRESULT in hex. `SignatureFailure` classes:

| Class | Provider results (winerror.h names) |
|---|---|
| `Tampered` | `TRUST_E_BAD_DIGEST` |
| `UntrustedSigner` | `CERT_E_UNTRUSTEDROOT`, `CERT_E_UNTRUSTEDTESTROOT`, `CERT_E_UNTRUSTEDCA`, `CERT_E_CHAINING`, `CERT_E_PURPOSE`, `CERT_E_WRONG_USAGE`, `CERT_E_REVOKED`, `CERT_E_REVOCATION_FAILURE`, `TRUST_E_EXPLICIT_DISTRUST`, `TRUST_E_SUBJECT_NOT_TRUSTED` |
| `Expired` | `CERT_E_EXPIRED`, `TRUST_E_TIME_STAMP` |
| `Malformed` | `TRUST_E_NOSIGNATURE`, `TRUST_E_SUBJECT_FORM_UNKNOWN`, `TRUST_E_MALFORMED_SIGNATURE`, `TRUST_E_CERT_SIGNATURE`, `TRUST_E_NO_SIGNER_CERT` |
| `Other` | anything else, including the case where the provider returns success but the leaf certificate, its name or its SHA-256 property cannot be read (reported as `E_FAIL`; the signature itself is fine) |

**Holding the file.** The library is opened for reading with read sharing only, and the open
`File` is in the verdict (`Signer::file`). A caller that keeps the verdict alive until the library
is mapped (`LoadLibraryExW`) stops anyone from overwriting, renaming or deleting the file in
between. The window before the check is covered by the writability check (ADR-270).

**Tests.** The header parser and the HRESULT mapping are tested on synthetic data on every
platform. The Windows tests sign a copy of the test executable with a throw-away self-signed
certificate through PowerShell (removed again afterwards), check an untrusted signer, a changed
byte, a Microsoft-signed system image, a writability finding together with an invalid signature, and
a real `LoadLibraryExW` of a copy of `version.dll` while the verdict holds the file. Where PowerShell or such an image is missing they are
skipped, and fail instead when the `CI` environment variable is set.
