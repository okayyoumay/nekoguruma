# ADR-266: Linux J2534 Registration Definitions Are TOML Files Keyed Like the Windows Registry

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** new crate `ngr-library-resolver`, design 7.1.1, ADR-228 items 2-4, `README.md`

## Context

On Windows a J2534 library is found through the registry: one key per device, with values
such as `Name`, `Vendor`, `FunctionLibrary` and one value per supported protocol. Design
7.1.1 gives Linux the equivalent as registration definitions, one file per VCI in the fixed
directory `j2534_definition_dir()` (`/etc/nekoguruma/j2534` by default, ADR-228 item 2), and
lists their content: name, vendor, the absolute path of the library, the supported protocols
and capability flags, the width of `unsigned long`, and additional search paths for dependent
libraries. It leaves the format open ("TOML or JSON") and says nothing about the keys, how a
name is matched, or what happens with a duplicate or a broken file. ADR-228 item 4 asks for
one shared crate that resolves a VCI name to a library path for the agent and the worker
services. Nothing reads the definitions yet.

## Decision

1. **TOML, one file per VCI.** Only regular files directly in the definition directory whose
   extension is `toml` are read; the file name is free. A symbolic link there is refused, and so is a
   definition directory that is itself a link (a junction too, on Windows), so a definition
   cannot come from outside the root-owned directory; its parent folders are left to the 7.2
   checks, and a file larger than 64 KiB is
   refused. TOML is what the workspace already
   uses for configuration, and it is easy to write by hand and from the generation helper.
2. **Keys are the Windows value names.** `Name` (required, not empty), `Vendor`,
   `FunctionLibrary` (required, absolute), `ConfigApplication`, and one key per protocol with
   the registry's value names (`CAN`, `ISO15765`, ...), each 0 or 1 like the registry's DWORD.
   Two keys exist only on Linux: `LongSize` (4 or 8, the width of `unsigned long` the library
   was built for, design 7.1.2) and `SearchPaths` (absolute paths). A definition maps onto
   the registry key one to one, so the two platforms share one vocabulary. Unknown keys and
   out-of-range values make the definition invalid, so a typo cannot pass unnoticed.
3. **The VCI is matched on `Name`, exactly.** A caller names a VCI and gets the definition
   whose `Name` equals it, case included. `Name` is the identifier: it stands for the Windows
   device's registry key name, which is what a Windows lookup matches, while the Windows
   `Name` value stays a display string. Names and paths with surrounding whitespace, paths
   with a NUL or a `..` component are invalid. Two valid definitions with the same `Name` are an
   error: the resolver refuses rather than pick one. An invalid file is skipped and logged;
   when a name is not found, the error says how many invalid files were skipped. A missing
   definition directory is the same as an empty one.
4. **The crate is `ngr-library-resolver`.** It parses and reads the definitions on every
   platform, so the format is tested everywhere, and resolves through them on Linux and
   through the registry lookup of `j2534-0404-registry` on Windows. The 7.2 checks and moving
   the existing lookups into it come later; until then its users keep their own lookups.

## Consequences

- The definition directory is root-owned (ADR-228 item 2), so only an administrator, or the
  generation helper running as one, adds a VCI on Linux.
- A capability flag beyond protocol support has no key yet: the registry has none, and one is
  added here when a need appears.
- A definition with an unknown key from a newer format is refused by an older build rather
  than half understood.
- Until the switch-over, `vci-discovery` and `j2534-0404-registry` keep their own Windows
  lookups, so the crate's Windows resolution duplicates theirs for a while.
