# ADR-270: What "Writable by Regular Users" Means for the Pre-Load Check

**Date:** 2026-10-10
**Status:** Accepted
**Affects:** `ngr-library-resolver` (crate root, `docs/library-resolver.md`), design 7.2, ADR-228 item 4; later the worker services and the agent when they switch over to the shared resolver

## Context

Design 7.2 requires that a vendor library, its folders and the files that name it are not
writable by regular users, and that device mode refuses to load a library that fails; ADR-228
item 4 places the check in the shared resolver crate. The wording leaves open what "regular
users" and "writable" mean on each platform, and a literal reading does not hold on real
systems:

- Default Windows ACLs on a volume root and on `%ProgramData%` let every user create files and
  folders there, so "no write right for regular users anywhere on the chain" refuses every
  library on every Windows machine.
- `/tmp`-style directories are world-writable with the sticky bit; paths contain symbolic links
  (for example merged-usr layouts where `/lib64` points into `/usr`); POSIX ACL grants are not
  visible as owner bits.
- Nothing in the code models user mode and device mode yet, and the agent wants to report a
  library's state in user mode too.

## Decision

1. **A set of trusted owners.** A policy names the principals allowed to hold write: on Linux
   uid 0; on Windows SYSTEM, BUILTIN\Administrators and TrustedInstaller. Every entry on a
   checked chain must be owned by one of them. A debug-only policy also trusts the current user,
   for tests and for the debug-only overrides of ADR-228 item 1.
2. **Files, links and the library's own directory** grant no write, delete, permission or
   ownership change to anyone outside the policy: on Linux no group or other write bit (no
   exception for group 0, so a POSIX ACL grant, which shows in the group bits, is caught); on
   Windows no allow entry for another principal carrying such a right.
3. **Further ancestors are containers.** Rights that let someone replace or remove an entry
   (delete, delete child, permission or ownership change) are findings; rights that only add new
   entries are accepted, which is the sticky bit on Linux and the add-file and add-subdirectory
   rights on Windows. The library's own directory gets no such allowance, since a planted
   sibling library is the sideloading path.
4. **Links are followed component by component**, with a hop limit: the link entry is checked
   for its owner, its target for everything. The library-directory rule goes to the directory
   that holds the final file name as the walk reaches it, not to the lexical parent of the given
   path. A `..` in the given path is a finding, and on Windows also in a link target, because
   Windows removes `..` by text before following links, so the walk could otherwise check another
   file than the one loaded. A relative path, a null DACL, an access-control
   entry of a type the check does not know and security information that cannot be read are
   findings; deny entries are ignored, which can only make the check stricter than Windows is.
5. **The check reports and the caller decides.** It returns every finding (path, role, reason)
   rather than the first, and takes no operating mode: device mode refuses with the findings as
   the reason, user mode, which runs as the logged-in user and crosses no boundary, logs them
   and loads. The Linux registration definition is a naming file; a Windows registry hit names
   none, since HKLM is the trust premise of 7.2; the worker service adds its configuration
   file.

## Consequences

- A vendor library under a directory regular users can add to (for example
  `%ProgramData%\Vendor` or a folder on a volume root with inherited default ACLs) is refused in
  device mode until an administrator tightens its ACL; the findings name the entry and the
  right.
- A root-owned file that is group-writable is refused even when the group is root's.
- A pass means no regular user can alter the chain, so the window between the check and the load
  cannot be used by one; only trusted principals could race it.
- The check does not cover the additional search paths of a Linux definition, which decide
  dependent libraries; they need the library-directory rule wherever a service applies them.
