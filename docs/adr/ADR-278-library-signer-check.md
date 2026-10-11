# ADR-278: Library Signer Check

**Date:** 2026-10-11
**Status:** Accepted
**Affects:** `ngr-library-resolver` (`signer` module, `Reason`, crate root, `docs/library-resolver.md`), design 7.2, ADR-228 item 4, ADR-270; later the worker services and the agent when they switch over to the shared resolver

## Context

Design 7.2 says that when a library carries an Authenticode signature its signer is verified. It
leaves open what "carries a signature" means, which trust source decides, whether revocation and
time stamps count, what happens on other platforms, and who decides between refusing and
loading. ADR-270 settled these questions for the sibling writability check (the check reports,
the caller decides by mode); the signer check needs the same kind of decision, and ADR-228 item 4
puts both pre-load checks in the shared resolver crate.

## Decision

1. **Presence is a non-empty security data directory in the PE header.** Only an embedded
   Authenticode signature counts. A file whose directory is empty, absent or unreadable as a PE
   image is `Unsigned` and goes on to the other checks; catalog signatures (which sign most
   operating-system files) are not looked at, so a catalog-signed file is `Unsigned` too.
2. **A present signature is verified with `WinVerifyTrust`**, the generic Authenticode policy,
   no user interface, no revocation check, and cache-only URL retrieval so the check never
   touches the network. Revocation is switched off explicitly (the no-revocation provider flag, not
   only the revocation-checks field), because the machine-wide Software Publishing policy can
   otherwise turn it back on for every caller; a revoked or revocation-failed result that still
   comes back is classed as an untrusted signer. A signature that verifies but whose leaf
   certificate, name or SHA-256 property cannot be read is reported as an invalid signature of
   class `Other` (the signature itself is not malformed). The lifetime-signing flag is not set: a signature with a trusted time
   stamp stays valid after its certificate expires, one without a time stamp does not. Only the
   primary signature is examined. The trust source is the operating system's certificate store.
3. **No pinning.** Any signer the store trusts is accepted. A successful verdict reports the
   signer certificate's simple display name and its SHA-256 thumbprint (upper-case hex), so a
   caller can log them, report them in `capabilities`, or pin later.
4. **Any failure is a finding on the library.** A non-success result for a present signature is
   one `Finding` (role `Library`, reason `InvalidSignature` carrying a coarse class and the raw
   HRESULT). The class separates a changed file, an untrusted signer, an expired certificate or
   time stamp, an unusable structure, and the rest. The check takes no operating mode: device mode
   refuses the library, user mode warns and loads, as in ADR-270 item 5.
5. **Other platforms report `NotApplicable`**; the call is the same everywhere.
6. **`check_library` is the single pre-load entry point.** It runs the writability check and the
   signer check and returns every finding together (ADR-228 item 4). A failure to read the file for
   the signer check is kept apart from the findings, since nothing is known about the signature. A
   failure does not discard what the signer check learned: the error carries the signer verdict
   (with its open file) when the signature was not found invalid, and the open file when it was,
   so a caller that warns and loads (user mode) loads the file that was checked.
7. **The file is held from the check to the load.** The check opens the library with read
   sharing only and returns the open file in its verdict. A caller that keeps it open across the
   load blocks overwriting, renaming and deleting the file until it is mapped, which narrows the
   window between check and load beyond what ADR-270 already accepts.

## Alternatives considered

- **Revocation checking of the whole chain with cached data only.** Rejected: cached data is
  usually absent for an offline device, so the result would depend on what happened to be cached;
  a check that sometimes cannot answer is no better than none, and an online check would make
  loading depend on the network.
- **Pinning to a vendor through a profile or configuration now.** Rejected for now: the profiles
  (design 9.3) do not exist yet, and a pin read from a file regular users can change would add
  a weaker trust root than the store.
- **Verifying catalog signatures too.** Rejected: it would call catalog lookups for every
  operating-system dependency, and vendor libraries that matter for the 7.2 premise are signed
  in the file.
- **A chain policy of our own over a private root store.** Rejected: a private store has to be
  distributed and kept current; the system store already is.
- **Findings only, without a separate `check_signer`.** Rejected: the agent wants the signer's
  identity for a signed library that passes, which a findings list cannot carry.

## Consequences

- A library whose certificate has expired and that has no time stamp is refused in device mode
  until it is re-signed (a residual; an administrator-only override is deferred until there is
  field evidence).
- A key that has been revoked is not detected. This is deliberate (decision 2); the writability
  check, not the signature, is what keeps regular users from planting a library.
- The signer check gives no protection against someone who can write the file: they can strip the
  signature, and a file without an embedded signature is `Unsigned`, which passes. The
  writability check is the gate (ADR-270). The signer check adds assurance about who signed a
  library that regular users cannot change, and catches a file that was altered, or re-signed with
  an untrusted key, while keeping a signature.
- The PE headers are located through `e_lfanew` (the DOS header is read first, then the headers at
  that offset), so their position in the file does not matter; an `e_lfanew` above 16 MiB or past
  the end of the file counts as no signature.
- Roots that a user imported into their own store are honoured in user mode, which crosses no
  boundary.
- Pinning can follow from an administrator-only source once profiles exist.
- The Windows code links `wintrust` and `crypt32`; the `worker-check` job type-checks the crate
  for the Windows worker targets so a mistake there shows up in CI.
