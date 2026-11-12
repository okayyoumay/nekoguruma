# ADR-417: A Job Keeps the Session of Its Own Targets Alive

**Date:** 2026-11-04
**Status:** Accepted (items 1, 4, 5 and 8's last sentence superseded, item 6's trigger replaced and item 9 amended by ADR-418: the worker sends the tester-present and the agent decides when)
**Affects:** `agent` (`link.rs` `LinkConfig`, `host.rs`, `policy.rs` unchanged, `runner.rs`, `restart.rs`,
the communication log's `Origin::KeepAlive`), design 8.2.4, 8.2.5 and 8.10; ADR-264 item 4;
ADR-325 items 8, 11 and 12; ADR-389; ADR-392; ADR-393; ADR-416

## Context

The agent sends no TesterPresent on a job's own link (ADR-264 item 4): a non-default session
survives only through requests the procedure sends. Any gap between a session change and the
next request that is longer than the ECU's session timeout drops the session and anything that
depended on it, and the next request is refused. The gaps are a `Wait`, the server round trip of
a seed-key computation (design 8.10), an HMI response, a slow worker, and the reads of the
restart's step 4b-3 after its replay entered the programming session. The policy keeps
TesterPresent from the VM because a job may keep a session alive only once it owns that
session (`policy.rs`); nothing the agent does about the session depends on the VM sending it.

ADR-325 item 11 already has the mechanism for access targets: the agent sends a TesterPresent
itself, with the positive response not suppressed, from a task of its own, sharing the link with
the VM one exchange at a time. It closed with a note that the same mechanism can keep a single
target's own session alive, "decided separately". This ADR is that decision (the maintainer's,
2026-11-04). The maintainer confirmed three choices the same day: the response window is the
link's send-receive ceiling (item 5), the keep-alive is on by default (item 2), and the period
counts from the last exchange on the link (item 4).

## Decision

The mechanism is ADR-325 item 11's, reused; each point below says where it comes from and what
differs.

1. **The agent keeps a non-default session alive on a job's own link.** While a non-default
   session is in force on a link of the job, the agent sends `3E 00` (TesterPresent, ISO 14229-1
   (2026) 9.7, sub-function zero, positive response not suppressed) on that link from a task of
   its own on the runtime, so it goes while the VM runs and while it waits on a `Wait`, the
   server or an HMI response (ADR-325 item 11). It applies to each link of the job that is not
   an access target's; an access target keeps ADR-325 item 11 and its loss rule (item 12)
   unchanged. A procedure has one link today; with several targets (ADR-325 item 8) each link
   has its own record and its own keep-alive.
2. **Settings.** `LinkConfig` carries the keep-alive's on/off and its period. It is on and the
   period is 2 s by default. A period that is not positive or is shorter than 200 ms is refused
   (the tolerance ISO 14229-2 (2021) 9.5 allows a server's session timer, which is the floor
   ADR-325 item 5 gives access targets). No upper bound is checked: a plain job declares no
   session timeout, and a period longer than the ECU's is the author's setting to get wrong, as
   a period of a declared access target would be if it were not checked there. Off restores the
   behaviour of ADR-264 item 4 for that link.
3. **How the agent knows a non-default session is in force.** The agent keeps, per link, the
   session its last positive answer to a DiagnosticSessionControl on that link confirmed. A
   positive answer to a session other than the default one (ISO 14229-1 (2026) 9.2.1) starts
   the keep-alive, a positive answer to the default session ends it, and so does a positive
   answer to an ECUReset (an accepted reset ends a session, ADR-264 item 2). The silence of a
   request whose positive response is suppressed is its result (ADR-408), so such a session
   control counts as answered positively. A negative or missing answer changes nothing in
   the record. The record starts as the default
   session whenever a link opens, and a reopening of the links starts it over (ADR-325 item 11),
   so a link opened again is never kept alive until the procedure enters the session again. This
   is also why the restart's teardown (ADR-264) is not disturbed: its link starts in the default
   session, its hard reset or its passive wait sends no session control, and the keep-alive
   begins only when the replay's own session control is answered. The agent tracks what it saw
   on the link and does not read the ECU's session from the ECU.
4. **The link is shared one exchange at a time.** The keep-alive and the VM's requests on a link
   take it in arrival order, one exchange at a time (ADR-325 item 11; ISO 14229-2 (2021)
   10.1.4.2 sends a physically addressed TesterPresent only while no other request is
   outstanding). A keep-alive that falls due while a step's exchange runs waits for it, and a
   request of the VM waits at most one keep-alive exchange; neither is refused with `LinkHeld`,
   which stays the refusal for a primitive nobody settled (ADR-389, ADR-416). The period counts
   from the last exchange on the link: it restarts when any exchange on the link ends, the VM's
   or a keep-alive, at its final response or at its failure, and the next keep-alive goes once
   a full period has passed since then and the link is free. This departs from ADR-325 item 11,
   which measures from the previous probe's send; access targets keep that rule unchanged. The
   ground is the server's session timer: ISO 14229-2 (2021) 9.5 stops it while a request is
   being processed and restarts it when the final response is complete, so every request, not
   only a keep-alive, restarts it. A long exchange such as an erase with a response-pending
   chain therefore holds the keep-alive off until it ends, and a link with frequent VM traffic
   sends no keep-alive at all. Unlike ADR-325 item 12 nothing here counts a long exchange
   against the target, since no loss clock exists.
5. **The answer and its window.** Only a final response counts (a response pending is none; the
   worker waits it out, ADR-325 item 11), and only a positive one. The window is the link's
   send-receive ceiling (`LinkConfig::send_recv_ceiling`, ADR-235 item 6), because a job's own
   link has no declared tester-present window as an access target has (ADR-325 item 5); the
   host ends an unanswered keep-alive at its ordinary deadline, with the cancel and settlement
   path of any primitive (ADR-416). No send time is kept and no loss clock runs: ADR-325 item
   12's dating of credited answers exists to age a clock, and this keep-alive has none, since
   its first failure is the loss (item 6).
6. **A lost or negative answer is a link error of the step in progress.** A keep-alive that gets
   no final response in its window, or whose final response is negative, is a failure of the
   link, latched: a later good answer does not restore it, since the ECU may have fallen back to
   its default session and relocked meanwhile and a TesterPresent is answered in that session
   too (ADR-325 item 12). The loss is reported to the VM by its next call on the host, whatever
   that call is. The keep-alive exchange and the VM's calls are never concurrent (item 4), so
   the step in progress is one of:
   - a call waiting behind the keep-alive: a request not yet sent, refused before it is sent as
     a request that was never sent (ADR-389), so it may not have reached the ECU;
   - a `Wait` in progress: the host polls a wait at the job's poll interval, so the next poll
     ends it with the error;
   - a server round trip in progress (seed-key, HMI): it is not interrupted, and the error is
     the next call the VM makes after it, the key's send for a seed-key;
   - between steps: the next call the VM makes, since the loss is latched and no call is exempt.
   A loss found after the VM's last call, with the job ending, is not reported: the keep-alive
   stops when the job's last step ends. The run ends at once in the host error and the in-run
   repeat of ADR-392 does not send the step again, since a resend meets an ECU whose session
   may be gone; the in-run continuation after a lost VCI (ADR-393) is not taken either, since
   the link did not leave. The journal is as for any host error.
7. **The restart follows design 8.2.5.** A lost session needs no rule of its own there: the
   restart already treats the ECU's session as unknown, because it must after any worker crash
   or VCI disconnection. Its identity and safety gates, its teardown (one hard reset, or the
   passive wait for the declared session timeout and margin), its confirmation of the default
   session through F186 and its state check apply as they are. A plain procedure repeats from
   its start or resumes at a checkpoint only where everything reachable is safe to repeat
   (ADR-335), which re-establishes the session through its own steps.
8. **The VM still may not send TesterPresent.** The policy is unchanged: the keep-alive is the
   agent's message, not the procedure's, so `check_request` still refuses the service for the
   VM and for a read-only job (ADR-325 item 11). A read-only job never has a non-default session
   on its own link, since it may not send a session control outside an access program, so the
   keep-alive goes there only on access targets' links, by ADR-325. The workers' cyclic
   tester-present of ISO 22900-2 (2022) Annex B stays off: the agent sets none of its ComParams,
   for the reason of ADR-325 item 11 (a second sender would make answers unattributable).
9. **The log shows it.** Each keep-alive is an ordinary entry of the communication log with
   `origin: keep_alive` (design 8.11), with the target of its link and the exchange's end as
   any exchange has. A VM request refused because of a latched loss is an ordinary refused
   entry. At the default period this adds one entry every 2 s while a session is in force.

## Consequences

- The sentence in ADR-264's Context and item 4 that the agent runs no TesterPresent holds only
  for a link with no non-default session in force, which is the state the teardown starts in
  (item 3).
- A keep-alive that is due while a TransferData block or any other exchange of the VM runs
  waits for it; a long job of small exchanges gets keep-alives between them, which an ECU
  tolerates as a TesterPresent between transfer blocks.
- With the session record only as good as the answers the agent saw, a session that the ECU
  dropped by itself (a relock or reset answered again in time) is not detected as a loss: the
  keep-alive is a TesterPresent, which is answered in the default session as well. Only a
  missed or negative answer ends the run. This is ADR-325's residual for a TesterPresent probe
  (its Consequences), accepted here; a session read on the job's own link is not part of this
  decision.
- A link whose window is the send-receive ceiling can hold the link for the ceiling when a
  keep-alive is unanswered, and the VM's calls wait that long before they fail. The ceiling is
  longer than a declared window would be; a declared window for a plain job's keep-alive is
  not part of this decision.
- Like ADR-325 item 11, this needs a worker that ends a running send-receive on cancel (its
  Consequences).
- Design 8.2.5 and 8.10 describe the S3 keep-alive during waits as a property of the agent;
  they now say which link has it and what a loss does.
