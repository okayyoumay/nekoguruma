# ADR-436: An Exchange With a Pending Open Owns Its Late Final Answer, Whatever Its End

**Date:** 2026-11-08
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `Evidence::late_final_settles`,
`LinkState::release_kept_ended`; `comm_log.rs` `late_response_owner`, the entry's keep in
`CommLog::exchange`), design 8.11 `primitive_ended`, `late_response`; ADR-366 decision 1,
ADR-429 decision 3, ADR-431 decisions 2 and 3, ADR-435 decision 4

## Context

A collecting exchange (the presence request) whose primitive ended with a lone pending open
and the finished status, but no receive timeout, stayed kept and owed (ADR-429, ADR-431): its
owed rows named the pending's identifier. Its ECU's final answer on that identifier was still
`unattached`, because `late_response_owner` owned a response only for an exchange whose
evidence had `vci_ended`, and the host sets that only for a read receive timeout. A finished
status without one is a wait the VCI did not end by timeout, so the exchange could be kept
and owed forever with nothing able to settle it. The entry written when the host's own wait
(no agent deadline) read the pending and then the finished status had the same shape and was
not even kept. The runner test of a pending read in a resend's wait hid this behind an extra
receive-timeout item.

An open pending is the ECU's own statement that its final answer is still to come, so the
missing receive timeout does not contradict it; the exchange is owed the answer whatever
ended the primitive.

## Decision

1. **`Evidence::late_final_settles` is `vci_ended` or an identifier with a pending open**
   (`open_pending_ids` not empty), a test on the exchange. One predicate is shared by
   `late_response_owner` (the gate on a single owner, replacing the `vci_ended` test), by
   `LinkState::release_kept_ended` and by the entry's keep. `vci_ended` itself, and the entry's
   rows that depend on it, are unchanged.
2. **The end does not limit it.** An exchange a pending is open on stays kept and owed after a
   finished status without a receive timeout, a link error, a lost VCI, an unknown end or a
   cancel by the agent, as the release already kept it (ADR-429 decision 3); the cancel still
   settles nothing (ADR-366). An end with no pending open is released as before.
3. **The entry keeps it too.** A collecting entry with a pending open is kept on the same
   predicate, so a final answer after an entry with no agent deadline is its `late_response`.
4. **The test is on the exchange, not on the response's identifier.** A final answer on a silent
   identifier of the same request (ADR-435) settles that identifier as well, and the exchange
   stays owed and kept on the others, as it does for one the VCI ended.
5. **The judgement is unchanged.** The late window comes from the pending's window and is judged
   against the limit in force then. In time it is `system`; past it, `vehicle` only when the
   entry shows the VCI ended its wait at the limit (`timeout_at_limit`), else `undetermined`;
   a window still open is at least `undetermined`.

This supersedes, for an exchange a pending is open on, ADR-366 decision 1's "a late response
still settles only an exchange the VCI ended" and ADR-431 decisions 2 and 3's gate (the kept
exchange owns a response only when its evidence shows `vci_ended`).

## Consequences

- A single owner of a late final answer no longer needs a receive timeout. Two owners are
  unchanged: the answer is `unattached`, an unowned `late_response` names both, each at least
  `undetermined` (ADR-435 Consequences); the runner test of a pending read in the resend's
  wait now asserts that without the receive-timeout item, and a new test covers the single
  owner.
- An entry's finished time is dropped by the host for an error result, so an entry's late final
  cannot be judged at the limit where the same exchange's `primitive_ended` can: the entry's is
  `undetermined`, the end's may be `vehicle`. This is a host-side gap, not changed here.
- An ordinary exchange is unaffected: it has no open pending, so the predicate reduces to
  `vci_ended`.
- ADR-366 decision 1, ADR-429 decision 3, ADR-431 decisions 2 and 3 and ADR-435 decision 4 are
  marked in their Status lines.
