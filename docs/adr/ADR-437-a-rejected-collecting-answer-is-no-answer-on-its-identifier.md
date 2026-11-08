# ADR-437: A Rejected Collecting Answer Is No Answer on Its Identifier

**Date:** 2026-11-08
**Status:** Accepted
**Affects:** `agent` (`comm_log/exchange.rs` `Answer::is_rejected`, `Answer::is_response`,
`standing_answers`, `standing_answers_covering`, `collecting_end`, `Evidence::final_standing`,
`Evidence::owed_ids`, `Evidence::as_answer`, `Kept::answered_placed`, `AnswerItem`; `comm_log.rs`
`primitive_ended`, `first_new_answer`; `host.rs` `answer_items`, `exchange`, `presence`), design
8.11 `answers`, `outcome`, "Owed
request", `primitive_ended`, `late_response`, `crates/agent/docs/ngr-agent.md`; ADR-359 item 3,
ADR-429 decision 1, ADR-435 decision 7 and Consequences

## Context

A collecting request (the presence request) lists every result its window read as an answer. A
result the worker took but the agent cannot use (a wrong echo, or a negative response to another
service) is `positive_unusable` there (ADR-359). ADR-429 decision 1 made the entry's end the first
standing answer that is not a pending, so such a result could be the end, and could stand as the
final answer of its identifier: it folded a pending of that identifier before it, covered the
identifier and any unnamed pending, and kept the identifier from being silent.

That disagreed with ADR-359 and ADR-435 decision 7, which count a rejected result as no answer.
The flag that says so, `Evidence::end_rejected`, was set by the host from the result of the
primitive and cloned by `primitive_ended`, so the same answers gave different ends depending on
where the end was stated: a wrong echo before a usable answer ended the entry as
`positive_unusable` with a usable answer beside it, and counted its identifier as answered, so the
exchange was settled although the ECU had not answered; the late restatement forced the flag to
false. An earlier change that touched only the late restatement made the entry and the
restatement disagree, so the rule has to be one shared by every place that folds answers or
states an end.

## Decision

1. **A rejected answer is no answer on its identifier.** `Answer::is_rejected` is
   `End::PositiveUnusable`, which the host gives by the same test that tells a result unusable
   (the host folds the `Answer`s it builds, with no second predicate). A rejected answer stays in
   the list, but the shared folds read it as nothing: `standing_answers` lets a later rejected
   result leave an earlier pending standing, `standing_answers_covering` lets it cover no
   identifier and so no unnamed pending (ADR-433), and `Evidence::final_standing` is false for
   it, so its identifier is silent and `owed_ids` step 3 does not take it as an owner's answer.
2. **The end is the first standing answer that is neither a pending nor a rejected result;
   with none, the first rejected result's; with neither, no response.** Every kind of rejection
   is handled alike (a wrong echo and a negative of another service), and a window with only
   rejected results ends as `positive_unusable` with the bytes of the first of them. This amends
   ADR-429 decision 1.
3. **`end_rejected` is derived from the answers wherever an end is stated.** `collecting_end`
   returns the end and whether it is a rejected result. The host's entry, `primitive_ended`'s
   restatement and `Kept::answered_placed` take both values from it, and `Evidence::as_answer`
   sets the flag from the answer it stands for, so the answer histories and the per-answer
   classification see no answer head in a rejected one. A final answer delivered late therefore
   replaces a rejected end, and one that is read beside a usable answer leaves the flag false. A
   primitive that is not collecting keeps ADR-359 item 3's formula.
4. **The answers list keeps a rejected result with its own outcome.** `AnswerItem` carries
   `positive_unusable`, classified by the existing rows (`undetermined` by the
   `positive_unusable` row, and `system` when it came late, as any final response does), and the bytes of both its `response` and its
   `outcome` are redacted as a rejected result's are (ADR-359 item 1). The entry's
   classification, as the worst of its answers', can then be `undetermined`, or `system` for a
   late one, beside a usable end.
5. **ADR-435 decision 7's whole-table debt stays as a stated invariant.** An exchange whose end is
   a rejected result is owed on every identifier of the receive table. With decision 1 every
   identifier of such an exchange is silent or open, so the rule is redundant for a collecting
   request, and it stays as the statement of what a rejected end owes. A test asserts that
   `owed_ids` is the whole table whenever `end_rejected` holds.

6. **Timing models the VCI; attribution does not count a rejected result.** The VCI takes a
   rejected result as a response and ends its wait for that identifier's window (the worker
   matched the result and bound it to the primitive; the agent's host rejects it, by
   `Progress::answers`, which is stricter than the worker's match). So the window rules
   (`answer_windows`, `final_before`, `chain_from`, `answer_windows`' `open_pendings`, the late
   judgement) keep a rejected result
   as a final: it ends a pending's window, a final answer delivered late is judged against P2
   from its end, and it restarts the window of the next answer. Attribution (the end, the folds,
   `final_standing`, silence, the owed identifiers, settlement) does not count it, as decisions
   1 to 3 say. The two read the same list; the split follows what each models. The link errors
   the log reads for an end are ordered against the first answer that is neither a pending nor
   a rejected result, as the host orders them (`Answer::is_response`). A response that began
   and did not complete stays so only while no answer stands; an answer, a rejected result
   included, is the end as the host has it.

Presence (ADR-396) counts the answers that stand on an identifier of the receive table. A pending
left standing beside a wrong echo on one identifier gives that identifier two answers; the
identification report keeps one entry per distinct bytes and asks by identifier, so who answered
is unchanged.

## Consequences

- The table cases follow: a wrong echo before or after a usable answer leaves the entry's end the
  usable answer's and its identifier silent and owed, until a final answer delivered late names it;
  a wrong echo alone ends the entry as `positive_unusable` and the whole table is owed; a wrong
  echo after a pending on its identifier leaves the pending open.
- ADR-435's Consequences bullet that left open whether a rejected collecting answer leaves its
  identifier unanswered is closed by decision 1.
- A window whose only results are rejected and that ended by the finished status (no receive
  timeout) is not kept, since the host sets `vci_ended` only for a read receive timeout or a
  collecting window with an answer, so a usable answer read later is `unattached`. This gap
  existed before and is left to a later change.
- The identification report may list two presence entries on one identifier where a pending and a
  rejected result both stand; nothing reads their number.
