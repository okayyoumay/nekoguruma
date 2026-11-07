# ADR-425: A Stray Collecting Primitive's Answers Are Carried Into Its `primitive_ended` Entry

**Date:** 2026-11-06
**Status:** Accepted (Consequences' `late_item` gap closed by ADR-426)
**Affects:** `agent` (`host` `Stray`, `Progress`; `comm_log` `primitive_ended`, `late_item`,
`judge_windows`, `exchange` `Kept`, `standing_answers`); design 8.11 `primitive_ended`; ADR-386,
ADR-396, ADR-400

## Context

ADR-400 judges the answers of a collecting exchange (the presence request) each from its own
window and its entry's window from the last answer, and left three gaps in the settling entries of
the primitive the agent stopped waiting for:

1. The answers the primitive gives after the agent's deadline never reached the log. The stray
   was built with the plain single-answer expectation (`Progress::expecting`), so the wait for its
   end ran without collecting mode: with no answer before the deadline, the first answer after it
   became the primitive's response, the finished status was never recorded, and an error read after
   an answer ended the wait as the answer instead of failing the window (ADR-396).
2. `primitive_ended` judged the entry's timing from the transmit, with no window start, and ended
   the window on the receive timeout alone, so a window that ended on the primitive's finished
   status was judged as a late or early timeout.
3. `late_item` restated a collecting exchange with the same transmit-based timing.

## Decision

1. **The stray keeps its primitive's collecting mode.** `Stray` carries `collecting` and the
   functional link's receive table, and `Stray::take_progress` applies both, so a wait for its end
   reads exactly what the exchange's wait would have (ADR-396). The same holds for the stray of a
   start the worker did not answer (ADR-416).
2. **The host hands each collected result and the finished status to the log.** A collecting
   wait that keeps its reads pushes every result bound to the primitive as an `EndItem::Answer`
   (the raw `exchange::Answer`, its end and times as the exchange's entry lists an answer) and the
   finished status as `EndItem::Finished`; a pending also names the response identifier it came
   on. The log places each once, in read order (ADR-386).
3. **One function judges every answer and the entry's window** (`judge_windows`): for the
   exchange's entry, for `primitive_ended` and for `late_item`. A window depends only on the items
   before it, so the answers an exchange's entry already listed get the same results when a
   settling entry judges them again, and an answer read after the deadline is judged from the end
   of the one before it, the first from the last the entry had. The exchange keeps the placed
   order and the ends of its pendings and answers (`Kept`) for it.
4. **The settling entry restates the answers under ADR-396's one-answer rule.** It lists the
   entry's answers and then those read for the end. A lone pending the entry listed, whose
   identifier gets its final answer after the deadline, is that answer's pending and no answer of
   its own; the rule is the one generic fold (`standing_answers`) the host's list of a window's
   answers also follows. Each answer is given the history the link derives for it, as an
   exchange's entry does (ADR-400 decision 4), and counts for the later exchanges as the entry's
   answers do.
5. **A window with answers ends on the finished item, else on the receive timeout**, which wins
   when both were read. The entry's end is the first answer left standing after the fold, by the
   rule an exchange that ended on its own follows (ADR-396 item 4; one function,
   `collecting_end`, serves the host and the log), so a lone pending read after the deadline is a
   negative end; an error or a lost VCI keeps its own. A collecting end that delivers an answer
   is `system` as every end that does (design 8.11 `primitive_ended`), and a window that ended in
   an error lists its answers and is not judged at its limit. The entry's `vci_ended` follows the
   exchange's rule (one function): a receive timeout, or a finished status that came with
   answers and no failure.
6. **The settling entry takes what an exchange's entry takes from the answers.** The first
   answer read for the end that carries a transmit done gives the entry its transmit when it had
   none (placed before that answer's end, and the send is `tx_done_time`), by the function the
   exchange's entry uses. A link error read for a collecting end is before the answer only as
   the host orders it: the window ended on an answer, the entry had no final answer, and the
   error was read before the first final answer read for the end. The answer counts for later
   exchanges with no placed time, as the exchange's entry gives it.

## Consequences

- Supersedes ADR-400's Consequences on settling entries: a settling entry that re-judges a
  collecting exchange judges its timing from the last answer, and the finished item is read by
  `primitive_ended` and `late_item`; the answers of a stray collecting primitive after the
  agent's deadline are carried into its `primitive_ended` entry.
- The classification of such an entry is `system` whenever its end is an answer, whatever the
  answers' own timing, which stays in the answers' items; the entry's window is `timeout_at_limit`
  when the finished item is 1 s after the last answer, on the clock the answers are on.
- Not decided here: `late_item`'s own judgement of a late pending or start of a collecting
  exchange still measures from the transmit (decided by ADR-426); the answers read before a cancel took effect are not
  listed in the cancel's entry. They are open items.
- Decided later by the maintainer (2026-11-07): the collecting deadline does not extend with each
  answer. It stays one bound from the request. A window still receiving when it passes is given
  up on, and the answers read afterwards reach the `primitive_ended` entry by decision 2, so
  nothing the VCI reports is lost. The entry is then `system` whenever its end is an answer.
