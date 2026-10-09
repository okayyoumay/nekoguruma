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
2. **No bucket file is ever created for one VIN alone.** When the file of the bucket a job
   needs, or the last bucket file (`vehicle-fff.lock`), is missing, `take_vehicle` creates every
   missing bucket file in bucket order before it opens its own, and creates `vehicle-fff.lock`
   last, after syncing the directory on Unix, so that its presence means the other files were
   committed first (NTFS commits directory entries in creation order without a sync). The
   invariant is that which files exist never depends on a VIN: a file is only ever created as
   part of a sweep over all 4096, and the sweep is the same whichever VIN triggered it. A
   complete set is the normal outcome, not the invariant. A crash, a power loss, a failed sync
   or a file someone else created can leave the set partial; what survives is decided by the
   file system's write order, not by a VIN. A later take whose file exists locks it and creates
   nothing; one whose file is missing sweeps again. The listing of a directory in which a
   vehicle lock was ever taken therefore says nothing about the vehicles seen there; created one
   by one, the existence of a bucket file would hint whether a given vehicle had been there.
   Jobs that never take a vehicle lock create none of these files. The files stay empty and,
   like the other lock files, are never deleted.
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
   opened handle must be a regular file before it is locked (`GuardError::NotAFile`); on Windows
   a reparse point is refused. An entry someone else put at a lock file's path (a symbolic link,
   a FIFO, a device) then fails the take instead of being followed or hanging outside the cancel
   loop. This closes the uncancellable wait and locking through a link; it does not harden the
   multi-user model, since an agent user who wants to bypass the guards can run a job without
   them (ADR-257) and the first user to create a lock file owns it. The bucket sweep multiplied
   the names someone could pre-create by 4096, which is why the shared open path is hardened
   here.

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
  are created with the default permissions of the user whose take creates them, like the other
  lock files; the directory's permissions for every agent user remain an installation
  requirement (ADR-257).
- A sweep whose directory sync fails leaves `vehicle-fff.lock` uncreated and fails that take;
  the next take sweeps and syncs again. On a file system without a metadata journal a crash can
  lose an arbitrary subset of the files; the next take that misses its own file recreates every
  hole at once, whose creation time then says only that some vehicle of the lost buckets arrived
  then.
- The agent writes nothing VIN-derived to the device: no lock file's name, existence or content
  depends on a VIN. Two residuals remain. While a job runs, the locked bucket is visible to
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
