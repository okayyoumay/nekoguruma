# ADR-404: A Hung Test Fails the CI Job Instead of Holding It

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `.config/nextest.toml` (profiles `ci`, `ci-windows`, `ci-windows-parallel`),
`.github/workflows/ci.yml` (`core-linux`, `core-windows`)

## Context

Three times in two days the `core-linux` job stayed in its `cargo nextest run --workspace` step
for between twenty minutes and over three hours, where it normally ends in about five minutes:
on two pull requests and once on `main`. Each time the job held the workflow's concurrency
group, so the next commit's run stayed pending; a plain cancel did not end it, and only a
force-cancel did. A force-cancelled job keeps no log, so which test hung is unknown, and a re-run
on the same commit passed.

nextest's CI profiles set no time limit, and the workflow's jobs set none either, so a test that
hangs runs until GitHub's six-hour job limit. Nothing names it, and the job blocks every later
run of the same branch.

## Decision

1. **The CI profiles end a hung test.** `ci`, `ci-windows` and `ci-windows-parallel` mark a test
   slow after each 60 seconds and end it after five such periods. nextest then reports the test
   by name as timed out, and the run fails. The slowest test on the Windows runner takes about
   55 seconds on `main` (the agent's restart-step tests beside the restart end-to-end scenarios),
   and the Linux ones less, so five minutes leaves more than five times the longest time seen.
   Ending a test this way is not a retry and does not loosen it: a test that hangs is a failure
   to find and fix, as a flaky test is.
2. **The test jobs have a backstop.** `core-linux` has `timeout-minutes: 45` and `core-windows`
   `timeout-minutes: 90`, far above their usual five and twenty minutes, so a step that hangs
   outside the tests (a build, a cache step, the doc tests) still ends.
3. **The hardware profile is unchanged.** Hardware checks run by hand, against a real VCI whose
   timing the profile does not bound.

## Consequences

- The next hang fails the job and names the test, which then goes under Known Flaky Tests and is
  root-caused there. Until then the cause of the three hangs stays unknown.
- A test that legitimately grows past five minutes fails; it should be split or made faster, or
  the limit raised here with the reason.
- The other jobs (builds, the ABI round trip, fuzz) keep GitHub's default limit; none of them hung.
