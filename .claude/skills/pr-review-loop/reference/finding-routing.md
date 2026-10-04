# Finding routing guide

Decision guide for Step 2 of the [PR review loop](../SKILL.md). Use it once a finding is
confirmed new (Step 1) and you have read the code it points at. Pick exactly one path.

## 2a. Direct fix

**When:** the root cause is clear, the fix follows a pattern already used in the same module, and
there is no design trade-off.

**Not when:** the finding has the same *shape* as one already fixed in this PR, whoever found
the first one (an earlier Codex round, `edge-case-hunter`, or you while implementing). A second
instance means the design that needed the first fix may be wrong; that is a 2b question even if
each fix is one line.

The one exception: the fix literally completes a pattern `design-advisor` already decided for a
sibling case of the same mechanism. Then send that `design-advisor` conversation a short,
grounded note (`path:line` of the new finding, the proposed completion in one sentence) and ask
whether the pattern is now complete or the same cause can surface elsewhere. Anything other than
a clear "complete" goes to 2b in full.

**How:** brief `implementer` precisely:

- the exact function and line, and the exact mechanism to apply (do not leave the mechanism to
  its judgment when you know it);
- what not to change;
- a regression test with **fail-without / pass-with proof**: revert the fix, see the test fail,
  restore it, see it pass. When the fix adds several independent checks, revert each one on its
  own; a combined revert can be masked by any single check;
- permission to say so when the exact race cannot be tested deterministically with the existing
  harness, rather than writing a flaky test.

## 2b. `design-advisor` escalation

**When:** there are several viable mechanisms or a real trade-off; the finding touches shared
locking, scheduling or a state machine; the fix's correctness depends on a non-obvious runtime
property (can two operations interleave, is a lock-order change safe); or a quick patch could
create the next finding. Do not hand a design question to `implementer` hoping it finds the
mechanism.

**The brief** follows the gate in `.claude/README.md` (facts collected, not an exploration
task):

- the finding, summarized in your own words with its `path:line`;
- your own trace of the root cause, with `path:line` for each claim;
- what you already tried or ruled out, and why;
- a request for a concrete, implementable mechanism, not a direction;
- any "touches too many call sites" claim measured first (a `grep` count by you or
  `code-scout`), so the costliest agent does not do cheap legwork;
- whether the fix changes an existing ADR. Amend the ADR that covers the mechanism in the same
  PR. If the verdict overturns an ADR's Decision, rewrite that Decision in this PR when the ADR
  was added by this PR; for an ADR already on `main`, supersede it with a new ADR (reserve the
  number with `adr-number-reservation`) and update the old one's Status, per CLAUDE.md;
- a request to flag any other latent gap it sees in the same code.

Ask for the whole surface at the first escalation, not the reported instance. The shapes that
recur:

- **A flag or guard's lifecycle** (set, clear, roll back): ask for the full state × event matrix,
  every place the guarded item can be (handler, queue, executing, done) against every event that
  can cancel, drop or complete it, with the required behaviour in each cell.
- **The same class twice, or the same mechanism twice** (the second instance from any source, or
  several differently worded findings in one function or tightly coupled group): ask for an
  enumeration audit, every variant × handler × side-effecting `.await`, success and failure arms,
  with pass/fail for each. Treat "the surface is closed" as a hypothesis: run `edge-case-hunter`
  on the audit's diff.
- **"State X is wrong during window W"** (a wait loop, a sleep, a non-cancellable phase): fix
  the whole window at once, every `.await` and sleep inside it, including nested branches.
- **"A check acted on state not yet updated"**: ask for every source of staleness for that
  field (a partial read of a backlog, a sibling task's separate copy, a value sampled earlier in
  the same loop iteration), and for each field whether its stale reader is destructive (needs a
  fix) or self-healing (a bounded, acceptable skew).
- **"A value computed once drifts from later configuration"**: check whether it should be read
  live or frozen at the event it describes, and if frozen, whether it is frozen as the raw
  identity (an address, a byte pattern) rather than a table label a later change can reassign.
  Check every sibling path for the same distinction.
- **A request the fix ignores or defers on purpose** (a cancel during a non-cancellable phase):
  list every reader of the marker it leaves behind (status queries, events, dispatch skips,
  drain paths) and decide each one in the same fix, or the status plane reports something false
  one round later.

## 2c. Accepted limitation

**When:** the gap is real, `design-advisor` confirms it cannot be closed without
disproportionate cost (for example holding a lock across hardware I/O), *and* the impact is
bounded and low: no leak, no corruption, no misrouting, nothing the P0 row of the backlog
priority table in `work/README.md` describes. A limitation that could reach the vehicle or an
ECU is never accepted here.

Do not decide "no cheap fix" yourself: two findings that look alike can differ in exactly the
lock or `.await` structure that makes one closable.

**How:**

- add a bullet to the relevant ADR's Consequences section with why it cannot close and what the
  bounded impact is (or a new ADR if none covers the mechanism);
- if closing it later is still worth doing, add a backlog item with the `backlog` skill;
- reply on the thread with the reasoning and react 👎;
- when Codex re-raises it after nearby edits, reply with a pointer to the ADR bullet.

## 2d. Decline: the mechanism does not happen

**When:** traced through the actual code, the claimed failure cannot occur. This removes the
safety net a fix would have given, so the bar is higher than for 2a or 2c.

- **P0/P1 findings are never declined on your trace alone.** Get an independent second trace
  from `design-advisor` (continue an open conversation on the same mechanism if there is one),
  asking it to verify or refute your trace. If the two traces disagree, find out which one is
  wrong; do not decline on a split verdict.
- **Trace exhaustively.** The bar is "no code path can realize this": enumerate every call site
  of the function involved, not just the path the finding names.
- **Record the trace.** Put the citation trail in the PR reply, and in the related ADR when the
  same claim is likely to come up again. React 👎.
