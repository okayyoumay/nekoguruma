# ADR-402: The Identification Job Narrows Its Candidates and Chooses the Next ECU

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `agent` (`identification`: `ecu_phase`, `read_definition_at`, `narrow`,
`contradictions`, `outcome_sets`, `next_definition`, `mark_unread`, the report),
`crates/agent/docs/ngr-agent.md`, design 8.8.2, `docs/glossary.md`; ADR-326 items 2 and 3,
ADR-327 items 3, 4, 5, 6 and 8, ADR-397, ADR-399 item 6, ADR-401 items 1, 6 and 11

## Context

ADR-401 read every definition in the plan's order and dropped no candidate, as an interim rule.
ADR-327 items 3 to 5 have the job drop a candidate only on a contradicting fact, stop reading
what no remaining candidate names, choose the next ECU by the worst case over its outcomes, and
end when nothing in play is left; ADR-399 item 6 left the VIN rules' drop to the same step. The
matching rules' fields are not read yet, so the only outcomes of a read are variant outcomes.

With variant outcomes only, the worst case never tells apart two definitions that can be
conclusive at every endpoint (both verdicts possible). The outcome
"definition D identified at endpoint E" drops a candidate C only when C does not list D, uses E
and has every definition it lists at E already ruled out there; the outcome "D ruled out at E"
drops a candidate only when some fact already identifies an unlisted definition at E and D was
the candidate's last unruled definition there. If both could happen for one D, the candidate
either lists the identified definition (which then matched, so not every definition of the
candidate was ruled out) or does not (and was dropped already). A definition with several
endpoints has mixed outcomes that drop nothing either. So the worst case of such a definition is
the number of remaining candidates, and the tie-breaks decide the order among them. A definition
with a refused variant can never be conclusive and has only the identified outcome, which can drop
a candidate, so it can score lower and be read earlier. The count gets teeth when the
rule fields land: a decoded rule value contradicts in every conclusive outcome.

## Decision

1. **Only `Selection::Candidates` drops.** A plan for a definition chosen by hand or for every ECU
   keeps every candidate, records no drop and reads every definition (ADR-327 item 8). All the
   items below that drop candidates are about a plan of candidates.
2. **The contradicting fact.** A read with a match identifies its definition at its endpoint; a
   conclusive read without a match rules it out there; a read without an endpoint, a read not
   made and an inconclusive one give no fact. A candidate is contradicted by an identified
   definition it lists in no entry, at an endpoint a definition of one of its entries is read at,
   counted only once every definition the candidate lists at that endpoint is ruled out there
   (ADR-401 item 4's conclusiveness). A candidate whose own definition there matched, is
   inconclusive, was refused, was refused by the worker or got no answer is kept. Facts persist:
   an identified definition that later leaves play still contradicts, since the server recomputes
   from all facts (ADR-327 item 6). A presence answer and a missing answer drop nothing.
3. **The VIN rules drop per component.** For each component a candidate declares VIN rules for,
   when the VIN is established and none of that component's rules matched, the candidate is
   contradicted; a definition lists several values of a component (ADR-326 item 2), so one
   matching rule keeps it. An unevaluated rule contradicts nothing. The drop is recorded with no
   read as its cause and shrinks what is in play before the first ECU read. This supersedes
   ADR-399 item 6's "nothing is dropped" for a plan of candidates.
4. **In play.** In a plan that does not narrow, every definition. In a plan of candidates, a
   definition an entry of a remaining candidate lists. A definition no remaining candidate names
   is not read; a definition is read at most once, so one a read ruled out is not read again.
5. **The next definition.** Among the definitions in play, not yet tried and with a group that
   opens a link, the one with the smallest worst case over its conclusive outcomes. The outcomes
   of a read are, per endpoint of the definition, identified there, or ruled out there where the
   read can be conclusive (no variant of the definition has a refused link); the outcomes of the
   definition are the combinations over its endpoints, and each verdict's drops are judged by the
   same `contradictions` function the run uses, so the choice and the drop cannot drift apart.
   Since a contradiction at an endpoint depends only on the facts there, the combinations are
   not built: the distinct remaining sets are folded endpoint by endpoint (`fold_outcomes`),
   deduplicated after each, which gives the same worst case without growing with the number of
   endpoints. No answer and an
   inconclusive read are not outcomes. A definition with no outcome counts every remaining
   candidate. Ties go to the definition named by the most remaining candidates, then the one
   with the most endpoints that answered presence, then the lowest identifier in byte order.
   With variant outcomes only the worst case is the remaining count for every definition for
   which both verdicts are possible at every endpoint (the finding in the Context); a definition
   with a refused variant can score lower. It is computed and reported, and the rule fields plug in
   as further per-endpoint alternatives of `outcome_sets`. The mixed outcome "identified at one
   endpoint, inconclusive at another" is not enumerated; with variant outcomes it cannot change
   the worst case, and the rule-field work decides whether it counts.
6. **The end.** The ECU phase ends when no definition in play is left untried, which is ADR-327
   item 5's "every definition still in play read or out of reach" and, in a plan of candidates
   with none remaining, "no candidate remains". A definition is tried after its own physical
   read whatever that gave (out of reach, inconclusive, refused by the worker); alternatives that
   share an endpoint are each tried, because the unit is the definition.
7. **The report.** `remaining` (the candidates no contradiction dropped; all of them for a plan
   that does not narrow), `dropped` (in order, each with its contradictions and `after`, the
   definition whose read completed it, null for the VIN), `read_order` (each step with the worst
   case that chose it) and the request outcome `NotInPlay` (the definition was not read since no
   remaining candidate named it when the reads ended). `ecu_reads` stays in plan order. An
   interrupted phase keeps the drops made, since a contradiction does not go away (unlike the
   VIN's agreement), and leaves the unread declarations of definitions still in play `NotRead`
   and the others `NotInPlay`; every entry stays `NotRead` as ADR-401 item 6 has it.
8. **Entries of dropped candidates** are judged from the same reads like any entry (ADR-401
   item 6), with no entry state of their own. `NotInPlay` counts as no physical read, so an
   entry that lists such an alternative is `NotRead`. An entry of a remaining candidate never
   sees `NotInPlay` at a normal end, since a definition a remaining candidate names is in play
   until it is tried.
9. **The order of a plan that does not narrow** is the same: the item 5 order replaces ADR-401's
   plan order for every plan, one code path. In such a plan nothing is dropped, so every definition
   has the same worst case, and the named-by count (zero in a plan for every ECU), the presence
   answers and the identifier decide.

## Consequences

- A plan of candidates sends fewer requests when a read or the VIN contradicts a candidate; the
  order is deterministic and no longer the plan's for any selection (ADR-401 item 1's plan order,
  item 11's "nothing is dropped" and the Consequences' "none is skipped" were interim).
- The worst case is reported but is the remaining count, for a definition that can be conclusive
  at every endpoint, until the rule fields are read, so the tie-breaks decide among those; a
  definition with a refused variant is inconclusive everywhere (ADR-401 item 4), has only the
  identified outcome and can score lower, so it can be read earlier. The count only counts
  variant outcomes until the rule fields land.
- A definition with a refused variant is listed at every endpoint a candidate's entries use and
  is never ruled out, so it keeps the candidate (`has_refused_variant`, shared by the read's
  judgment, the outcomes and the contradiction).
- An interruption narrows once more before it ends the phase: a match from a partly read
  definition counts as a fact (as ADR-401's match from fields decoded before the interruption
  does), while ruling out needs a conclusive read, which an interrupted read never is.
- An entry of a dropped candidate can be `NotRead` for a definition the run never needed.
- Drops survive an interruption while entry states do not.
- The server recomputes the candidates from the facts with the same rules; the contradiction is
  one function it can mirror.
- ADR-397: `read_definition_at` is the former loop body of `ecu_phase`, the only caller of
  `ecu_read_group`; `contradictions` is the one judge of a drop for the run and for the
  hypothetical outcomes.
