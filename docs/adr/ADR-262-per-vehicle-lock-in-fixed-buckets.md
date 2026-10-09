# ADR-262: The Per-Vehicle Lock Is One of 4096 Fixed Bucket Files

**Date:** 2026-10-09
**Status:** Accepted
**Affects:** `agent` (`src/guards.rs`, `docs/ngr-agent.md`), `Cargo.toml` (`sha2`), design 8.8, ADR-256 item 6

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
   `vehicle-{k:03x}.lock`, where `k` is the first two bytes of SHA-256 over the VIN's bytes,
   big-endian, masked to 12 bits. Many VINs share each file, so a name reveals 12 bits of a
   public hash and does not identify a vehicle. SHA-256 comes from the `sha2` crate, which the
   workspace already builds; the standard library's hasher is not stable across releases.
2. **All 4096 files exist from the start.** Preparing the lock directory creates every bucket
   file when the last one is missing, so the directory's content is the same on every device
   and does not show which buckets were ever used. Created lazily, the existence of a bucket
   file would hint whether a given vehicle had been there. The files stay empty and, like the
   other lock files, are never deleted.
3. **It is taken last, by `JobGuards::take_vehicle`.** The lock order becomes VCI, then the
   reprogramming slot when the job holds one, then the vehicle. Guards with or without the slot
   may take it. The wait polls and stops on a cancel like the other guards (ADR-256 item 3);
   a cancel leaves the locks the guards already held. Taking the bucket the guards already hold
   returns at once: the OS lock is per file handle, so a second handle in the same process
   would wait on the first forever. Taking a different bucket is refused
   (`GuardError::OtherVehicleHeld`), since a job targets one vehicle.
4. **It follows the job's guards.** The vehicle lock stays with the `JobGuards` across the
   runs of one job, as ADR-229 item 2 step 1 requires of a job that survives a worker crash.
   Guards marked `link_unconfirmed` keep it when dropped, like their other locks (ADR-258).
5. **The bucket count and the hash are part of the lock protocol.** Two agent builds that
   disagree on either share a directory without excluding each other. A change needs a new
   file prefix and counts as a breaking change of the lock protocol.

## Consequences

- Two vehicles whose VINs fall in the same bucket exclude each other: a job waits for the other
  vehicle's job to end. For two concurrent jobs this happens with a probability of about 1 in
  4096. It delays a job and never deadlocks: the order VCI, slot, vehicle is total, and a job
  holds at most one bucket. Design 8.8.1's "VCI-A on vehicle X, VCI-B on vehicle Y" holds
  except for such a collision.
- The lock directory holds 4096 more empty files, created once per directory.
- No VIN-derived fact is stored on the device, so the maintainer's condition on ADR-261 (no
  target VIN passed outside tests before the journal's protection) is unaffected.
- The restart's promotion to the vehicle lock at the first matching VIN, and the first run's
  VIN match among the execution preconditions (design 8.9.1), call `take_vehicle` later.
