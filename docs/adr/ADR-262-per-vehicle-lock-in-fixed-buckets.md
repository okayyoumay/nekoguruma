# ADR-262: The Per-Vehicle Lock Is One of 4096 Fixed Bucket Files

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/guards.rs`, `src/journal.rs`, `src/restart.rs`, `docs/ngr-agent.md`), `Cargo.toml` (`sha2`), design 8.8, ADR-256 items 1 and 6, ADR-257

## Context

Design 8.8's two-stage locking promotes a job from its per-VCI lock to a per-vehicle lock once
a VIN read matches the job's VIN. ADR-256 item 6 left the per-vehicle lock out of the job
guards because its lock file stays on the device, like every lock file (ADR-256 item 1), and a
file named after the vehicle would keep VINs on the device outside the retention and deletion
rules for personal data (design 5.5, 16.2). The file name must therefore not let anyone
recover the VIN.

ADR-256 item 6 gave a keyed digest as an example. A keyed digest needs a secret, and none fits:

- The lock directory is shared by every agent process on the device, whatever user it runs as
  (ADR-256 item 1, ADR-257), and every one of them must compute the same file name. The key
  would have to be readable by all of them, so it protects against none of them. A reader of
  the directory at rest finds the key next to the files.
- The journal's future key (design 5.5) comes from the OS credential store, which is per user,
  so it cannot name a file other users' agents must find.
- The key could never change: two jobs must agree on a name, and lock files are never deleted.

Without a usable key, a keyed digest is an unkeyed digest. A full-width unkeyed digest of a
VIN is a pseudonym that a reader can reverse by trying the serial numbers of a known model,
which is what ADR-256 item 6 forbids.

## Decision

1. **The vehicle lock is one of 4096 fixed files.** The lock of a VIN is the OS lock on
   `vehicle-{k:03x}.lock`, where `k` is the first two bytes of SHA-256 over the VIN's 17 ASCII
   bytes, read big-endian and masked to their low 12 bits (a digest starting `84 b1` gives
   `vehicle-4b1.lock`). Only a well-formed VIN (17 characters, each a digit or
   an upper-case letter other than I, O and Q, the form ADR-261 compares) is accepted
   (`GuardError::InvalidVin`), so two jobs on one vehicle always hash the same bytes. Many VINs
   share each file, so a name reveals 12 bits of a public hash and does not identify a vehicle.
   SHA-256 comes from the `sha2` crate, which the workspace already builds; the standard
   library's hasher is not stable across releases.
2. **No bucket file is ever created for one VIN alone.** Before it opens its bucket,
   `take_vehicle` lists the lock directory and counts the bucket files (`vehicle-` followed by
   exactly three lowercase hex digits and `.lock`). When fewer than 4096 are present, it creates
   every missing one, in bucket order, and syncs the directory on Unix, a best-effort step for
   durability only (a failed sync is logged and does not fail the take); then it opens its own.
   On Unix each file it creates is readable by everyone (mode `0644`) whatever the creating
   user's umask, since one sweep creates the file of every vehicle: a umask of `077` would
   otherwise lock every other agent user out of every vehicle lock until an administrator
   changed the files. A file gets its name only with its final mode: it is created as a staging
   file, given its mode and hard-linked to the bucket's name, which fails if the name exists, so
   a crash never leaves a bucket with the creator's umask; at most a staging file remains, which
   nothing counts or opens. The lock directory must therefore support hard links on Unix. The
   files are empty and their names carry nothing, and the directory's permissions still decide
   who can reach them. Whether a sweep runs therefore depends on the
   directory's state alone, never on the VIN, on every platform: a set left partial by a crash,
   a power loss, a failed sweep or a file someone else created is completed by the next take
   whichever vehicle it serves. The directory's listing says nothing about the vehicles seen
   there; the creation times of refilled files say only that some vehicle lock was taken then.
   No completion marker is inferred from creation order, and no file system ordering guarantee
   is relied on. Listing the directory requires read permission on it for every agent user, in
   addition to ADR-257's requirements; on Unix the guards refuse a directory that group or
   others may write but not read. Jobs that never take a vehicle lock create none of these
   files. The files stay empty and, like the other lock files, are never deleted.
3. **It is taken last, by `JobGuards::take_vehicle`.** The lock order becomes VCI, then the
   reprogramming slot when the job holds one, then the vehicle. Guards with or without the slot
   may take it. The wait polls and stops on a cancel like the other guards (ADR-256 item 3);
   a cancel leaves the locks the guards already held. Taking the vehicle the guards already
   hold returns at once: the OS lock is per file handle, so a second handle in the same process
   would wait on the first forever. Another VIN, even one in the same bucket, is refused
   (`GuardError::OtherVehicleHeld`), since a job targets one vehicle. Guards marked
   `link_unconfirmed` are refused as well (ADR-258).
4. **It follows the job's guards.** The vehicle lock stays with the `JobGuards` across the
   runs of one job, as ADR-229 item 2 step 1 requires of a job that survives a worker crash.
   Guards marked `link_unconfirmed` keep it when dropped, like their other locks (ADR-258).
5. **The bucket count and the hash are part of the lock protocol.** Two agent builds that
   disagree on either share a directory without excluding each other. A change needs a new
   file prefix and counts as a breaking change of the lock protocol.

6. **Lock files must be regular files.** Every lock file (`vci-*.lock`, `reprogramming.lock` and
   the bucket files) is opened without following a symbolic link and without blocking, and the
   opened handle must be a regular file before it is locked (`GuardError::NotAFile`, or
   `GuardError::NotAVehicleFile` for a bucket file, which names no path so that no bucket
   appears in an error); on Windows a reparse point is refused. An entry someone else put at a
   lock file's path (a symbolic link, a FIFO, a device) then fails the take instead of being
   followed or hanging outside the cancel loop. This closes the uncancellable wait and locking
   through a link; it does not harden the multi-user model, since an agent user who wants to
   bypass the guards can run a job without them (ADR-257) and the first user to create a lock
   file owns it. The bucket sweep multiplied the names someone could pre-create by 4096, which
   is why the shared open path is hardened here.

## Consequences

- Two vehicles whose VINs fall in the same bucket exclude each other: a job waits for the other
  vehicle's job to end. For two concurrent jobs this happens with a probability of about 1 in
  4096. It delays a job and never deadlocks: the order VCI, slot, vehicle is total, and a job
  holds at most one bucket. Design 8.8.1's "VCI-A on vehicle X, VCI-B on vehicle Y" holds
  except for such a collision.
- A job that holds the reprogramming slot and waits for its vehicle keeps the slot meanwhile,
  so a long job on that vehicle (or bucket), such as monitoring, delays every reprogramming on
  the device until it ends.
- The lock directory holds 4096 more empty files once a job has taken a vehicle lock there. They
  are owned by the user whose take creates them and, on Unix, readable by everyone (item 2); on
  Windows they inherit the directory's ACL like the other lock files. The directory's
  permissions for every agent user remain an installation requirement (ADR-257).
- The staging file is not synced before it is linked: a power loss during a sweep could
  persist a bucket's name but not its mode only on a file system that does not commit metadata
  changes in order, which ext4, XFS and btrfs do. Other agent users would then fail that bucket
  with a permission error until an administrator changed its mode. Syncing every staging file
  would cost up to 4096 synchronous writes in the first vehicle lock, which is not worth this.
- A sweep that fails partway leaves a partial set; the next take sweeps again. The cost of the
  trigger is one directory listing per `take_vehicle`. A shared lock directory that cannot be
  listed (for example mode `1733`) is refused on Unix, and fails every vehicle lock with a
  permission error elsewhere; the installation must make it readable by every agent user.
- The agent writes nothing VIN-derived to the device: no lock file's name, existence or content
  depends on a VIN. Neither `JobGuards`' `Debug` output nor the guards' errors name a bucket, so
  no log keeps one. Two residuals remain. While a job runs, the locked bucket is visible to
  anyone who can inspect open files, like the job's process itself. On a Windows volume that
  maintains last-access times (off by default on system volumes of 128 GB and more since Windows
  10 version 1803, and off before), a bucket file may keep the time it was last opened for a
  lock, if NTFS counts an open without a read as access; this is to be measured, and suppressed
  per handle if it does. Either residual is 12 bits of a public hash with a time: it narrows a
  candidate list an observer already has by a factor of 4096 and identifies no vehicle without
  one, and the next lock on the bucket overwrites it. Linux keeps none: atime moves on reads,
  and the files are never read. The maintainer's condition on ADR-261 concerns the target VIN
  itself and is unaffected.
- The restart's promotion to the vehicle lock at the first matching VIN, and the first run's
  VIN match among the execution preconditions (design 8.9.1), call `take_vehicle` later.
