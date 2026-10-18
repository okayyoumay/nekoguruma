# ADR-327: Vehicle Identification Runs on the Agent

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `agent` (an identification job; the variant guard of jobs that change an ECU),
`server` (identification plans, re-evaluation of the reported facts, guard lists),
`diag-frontend` and `diag-ir` (operator-facing variant attributes, definitive negative
responses, the read-only check of identification services), design 8.2.4, 8.2.5, 8.8.2, 8.9.1;
ADR-229 item 2, ADR-235 items 3 and 8, ADR-244's record set, ADR-315 item 3, ADR-318 items 1 and 4 and its Consequences, ADR-324 items 1 and 2, ADR-325 item 7,
ADR-326 items 2, 3 and 5 and its Consequences; `docs/glossary.md`

## Context

ADR-326 describes vehicles by vehicle definitions: identification values with their matching
rules, and a topology of ECU entries whose alternatives are ECU definitions. It leaves the
server to identify a vehicle from values a read-only job reports, and says the agent does not
use vehicle definitions. ADR-325 lets a procedure address several ECUs as targets; a job holds
the declaration-only documents of every candidate variant of every target, identifies each
target's variant before the program runs, and runs the program lowered for the combination
identified.

Identifying the configuration of a connected vehicle takes many reads, each chosen from what
the previous ones showed: which ECUs answer, which variant each one is, and so which vehicle
definitions remain possible and which ECUs are still worth reading. Driving that loop from the
server costs one round trip per step and does not work while the device is offline (design 5.7).

The maintainer set the procedure: check which ECUs are present with functional requests; narrow
the candidate vehicles from what the ECUs report; drop the ECUs no remaining candidate can carry;
read the next most useful ECU; repeat until every ECU is read, none is left to read, or the rest
are out of reach. When several models or several variants of one ECU remain, the operator
chooses, for variants from information attached to them (for example manual or automatic
transmission). A manual model choice and an access to every ECU remain for unregistered vehicles
and misidentification. Whether the VIN is used is up to the author of the vehicle data. The
maintainer chose to run the loop on the agent.

## Decision

1. **An identification job runs the loop on the agent.** It is a read-only job kind that carries
   an identification plan, which the server compiles from the candidate vehicle definitions and
   signs with the job instruction. The plan's candidates are every vehicle definition the tenant
   may use that no value established from the vehicle contradicts; values the operator entered
   are left out when the plan is compiled and applied by the server afterwards (item 6), so an
   operator's entry cannot remove a model before the vehicle is read, and the server keeps the
   candidate set that ADR-326 item 5 evaluates a role's scope against. The identification job
   itself is authorized without that scope check, which cannot be evaluated before the values
   exist, and its plan is not narrowed by the operator's role, since a narrowed plan could
   identify an out-of-scope vehicle as one inside the scope; it is read-only, and the scope
   check applies to the jobs that follow. The plan holds, per
   candidate, its identifier, its identification values and their matching rules, and its ECU
   entries (role, alternative ECU definitions, optional flag, gateway entry); and the documents
   of every variant of every ECU definition the candidates name, referenced by hash as other job
   documents are (ADR-318), so variant identification and field decoding work as in any job.
   The agent still fetches no vehicle definition (amending ADR-326 item 5): the plan is all it
   reads, and the server can prefetch it with the job for offline work. The job holds the
   per-VCI lock and promotes to the per-vehicle lock when it reads a VIN (8.8); it sends only
   reads and presence requests. Every service that a variant pattern, a VIN source (item 5) or
   a matching rule's read field names must be in the read-only set of ADR-235 item 8 (its
   per-layer lists are ADR-309 item 4's): a document or vehicle definition that names another
   service is rejected when it is ingested, and a plan or job that carries one is refused before
   any link opens, so neither an identification job nor item 7's guard can itself be the first
   request that changes an ECU. These refusals do not depend on the build; the host's check of
   every request (ADR-235 item 8) stays a second checkpoint where ADR-247 leaves it in force.
2. **Presence first.** Before any other request, for each distinct bus and functional
   identifier that the declared links of the plan's variants give (a variant's protocol may
   declare one), the agent sends one presence request functionally (for UDS, TesterPresent with a
   response requested) and records each physical response identifier that answered. Unlike the
   single final response of ADR-235 item 3, this request collects every answer that arrives
   within the response window on any response identifier of that bus the VCI lets it receive;
   the window is no shorter than the longest P2 time (ADR-235 item 4) that the declared links on
   that bus and functional identifier give.
   An endpoint is a physical address: the bus (the link's resource as the agent resolves it,
   ADR-319: protocol, bus type and pins, with its baud rate, so two spellings of one stack that
   ADR-324 item 2 accepts name the same bus) and the physical request and response identifiers; definitions whose links differ only in timing or other
   ComParams that do not distinguish the network share an endpoint, since they can reach the
   same ECU. A functional answer shows only the bus and the response identifier, not which
   physical request identifier the ECU listens on, so a presence answer is recorded as that pair
   and counts for every endpoint with that bus and response identifier. Presence is keyed this
   way, not by ECU definition, because alternatives such as two suppliers' ECUs for one role
   often share an endpoint, and an answer says only that one of the definitions that could give
   it is fitted, until a read tells which. An answer is evidence of presence and drops no
   candidate, even when no candidate's entries could have given it, since a topology need not
   list every device on the bus (a third-party device, an aftermarket unit); it is reported
   (item 6). Silence is not evidence of absence, since an ECU behind a gateway, a sleeping ECU
   or one on a link without functional addressing may not answer. This is the one functional
   request the agent sends, and it uses the functional identifier the ECU definition's protocol
   declares, which ADR-324 item 2 leaves unused for a document job's link; that link still sends
   only physical requests. ADR-235 item 8's read-only policy forbids TesterPresent because it
   keeps a session alive that the job may not own; the presence request is excepted for an
   identification job only, and is sent once per bus and functional identifier, never repeated
   as a keep-alive. A
   functional request reaches every ECU on the bus, so it may restart the S3 timer of an ECU
   that is in a session another tester opened, when the ECU cannot tell the two testers apart
   (ISO 14229-2 (2021) 9.5 restarts the timer on any request from the client that opened the
   session); sent once, it extends such a session by at most one S3 period, as any single read
   addressed to that ECU would. The other TesterPresent requests stay forbidden to read-only
   jobs, apart from the agent's own keep-alive of access targets (ADR-325 item 11).
3. **Narrowing.** A candidate is dropped only when a fact contradicts it: an identified variant,
   on an endpoint that one of the candidate's entries uses, of an ECU definition that no entry of
   that candidate lists (the candidate expects another ECU at that address, which is stronger
   evidence than a presence answer, since an unlisted device at an address the candidate does
   not use drops nothing), or an identification value whose matching
   rule (a VIN position range, or a read field) gives a value outside the candidate's. An
   identified variant counts against a candidate only once every definition that the
   candidate's entries list at that endpoint has been read there conclusively without a match,
   since an ECU can match the patterns of two alternative definitions: a candidate whose own
   definition at that endpoint also matched is not contradicted, and one whose own definition
   is out of reach or inconclusive there (item 5) has not been ruled out, so it stays and, if
   more than one candidate is left at the end, the operator chooses.
   Conclusiveness is judged per definition and endpoint, over the variants whose declared links
   are on that endpoint: a read there is conclusive when every identification request of those
   variants got either a positive response that decodes or a negative response that the ECU
   definition declares, for that request, as proof that the ECU cannot report what its patterns
   expect. No negative response proves that by default: the same code (requestOutOfRange, for
   example) can mean an identifier the ECU lacks or one that is unavailable in the current
   session or state, so only the ECU definition's author can say which codes are definitive for
   that ECU. A variant whose declared link ADR-324 refuses cannot be read, and leaves its
   definition's read on that endpoint inconclusive. A conclusive read whose variants all fail to
   match rules out only that definition on that endpoint; a read with no variant matching in
   which some request got no answer or an undeclared negative response is inconclusive and rules
   out nothing, since ADR-315 counts those entries as failed. The other definitions sharing the
   endpoint stay possible, and an entry becomes an unrecognized variant (ADR-326 item 3) only
   when every alternative of that entry using the endpoint has been read conclusively without a
   match; when some of those reads were inconclusive and none matched, the entry is reported as
   answered but not identified, a state beside ADR-326 item 3's. When
   two alternatives of one entry both match, neither is taken as fitted: the entry is ambiguous,
   its state says so, and the operator picks the alternative, then the variant as item 7 says
   (amending ADR-326 item 3, under which the first matching alternative is fitted). A missing
   answer never drops a candidate. An ECU definition that no remaining candidate names, or that
   a read has ruled out, is no longer read.
4. **The next ECU.** Among the ECU definitions still in play (named by a remaining candidate, or
   in a plan without candidates, item 8, every definition in the plan) and not yet read or out of
   reach, the agent reads the one whose outcome leaves the fewest candidates in the worst case,
   counting as outcomes the distinct sets of candidates that a possible response would leave:
   each set of variants it may report together (variants on different declared links of one
   definition can match at once, and item 7 reports every match), combined with the values the candidates' matching rules expect
   from the fields read with it, grouped by the candidates the response satisfies (two
   candidates whose expected values overlap, such as one prefix of the other, stay in one
   outcome), and a conclusive match of no variant, likewise grouped by the candidates the values
   of its decoded rule fields satisfy. No answer and an inconclusive read are left out of the
   count: they narrow nothing (item 3), so they would leave every candidate for every definition
   and erase the differences between them; how likely an answer is enters only through the
   tie-breakers. Ties go to the one named by more remaining candidates, then to one with more of
   its endpoints matched by a presence answer (item 2), then to the lowest identifier, so the
   order is deterministic. A read is the definition's variant identification (8.9.1) and the
   fields the candidates' matching rules read from it, each variant tested over the link it
   declares (ADR-324 item 1, which gives each variant its own declared link): the variants are
   grouped by declared link and each group is identified on its link, so the job opens one link
   per distinct link it reads, one at a time (amending ADR-324 item 1's single link per job for
   this job kind). A matching rule's read field names its source as a VIN rule does (item 5): a
   service and field as one variant document of that ECU definition declares them, read over
   that variant's link (amending ADR-326 item 2, whose rule names only a field of an ECU's read
   service; service and field identifiers are local to a document, ADR-318). One response is
   decoded with the declaration of every rule source on that service and link for the same
   information component (ADR-326 item 2: OEM, vehicle model, model year or vehicle type; rules
   for different components are evaluated apart, since their values differ by nature), and it
   establishes that component's value only when every one of those declarations decodes it to
   the same value; when any of them fails to decode it, or decodes it to another value, that
   component stays unevaluated from this response and narrows nothing (item 3), since a layout of another variant could
   decode the response to a value that drops the fitted vehicle's candidate. The endpoints of a
   definition are those of its variants' links. Each ECU definition is read at most once per
   job.
5. **End, and the VIN.** The loop ends when every ECU definition still in play has been read or
   is out of reach, or when no candidate remains; a plan without candidates (item 8) ends when
   every definition in it has been read or is out of reach. Out of reach is established only by
   a physical read, per definition: a definition whose identification requests all go unanswered
   is out of reach, and one read inconclusively (item 3) is inconclusive; both count as tried for
   the end of the loop and keep the candidates that need them. Alternatives that share an
   endpoint may use different identification requests, so each one is still tried; the endpoint
   itself counts as out of reach only when every definition still in play that uses it has been
   tried without an answer. Silence to the presence request alone never puts a definition out of
   reach. A VIN matching rule names the source its VIN is read from (amending ADR-326 item 2,
   whose rule gives only the position range and expected text): a service and the field that
   carries the VIN, as the variant document that declares them defines them (service and field
   identifiers are local to a document, ADR-318), read over that variant's link. When a
   candidate declares a VIN matching rule, or the plan holds a definition chosen by hand that
   declares one (item 8), the VIN is read after the presence requests and before
   any ECU definition's read: every distinct service and link those rules' sources name is read
   in turn, in a fixed order. One response is decoded with the
   declaration of every source on that service and link (two sources whose declarations differ
   are distinct), and it establishes a VIN only when every one of those declarations decodes
   it to the same well-formed VIN (ADR-261 item 3); if any of them fails to decode it, or decodes
   it to something else, the response establishes none, since a response decoded with another ECU's
   layout can look like a VIN. The VIN is established only when at least one response
   establishes one and every response that establishes one gives the same VIN; two that differ
   (an ECU fitted from another vehicle may keep that vehicle's VIN) establish none and are both
   reported. When no VIN is established, every VIN rule stays unevaluated and the job does not
   promote to the per-vehicle lock. When neither a candidate nor a definition chosen by hand declares a VIN matching
   rule, identification reads no VIN (the data author's choice).
6. **The agent reports facts; the server decides.** The job reports the VIN when established and every VIN an accepted response gave, the
   presence answers, each read's decoded identification fields and matching variants, each
   rule field's established value or that it stayed unevaluated, the
   outcome of each of its identification requests (decoded positive response, the negative
   response code, or no answer, so the server can tell a conclusive read from an inconclusive one
   by item 3), and the entry states of ADR-326 item 3 with item 3's ambiguous state. The server
   recomputes the candidates from these facts with the same rules, so the agent's narrowing only
   chooses what to read, and then applies the values the operator entered (item 1). An identified
   variant that contradicts a candidate's topology (item 3) is a value established from the
   vehicle, so it counts for ADR-326 item 2's candidates and for item 5's scope check as the VIN
   and read fields do. One candidate is selected; several go to the operator, as ADR-326 item 2
   says, with ADR-326 item 5's rule that operator choices never widen what a role permits.
7. **Ambiguous variants, operator attributes and the guard.** A variant may carry
   operator-facing attributes, labels with values (transmission: manual), that say nothing to the
   ECU and serve only this choice. When more than one variant of an ECU definition matches in an
   identification job, the agent reports all of them instead of the first one ADR-315 item 3
   selects, and the operator picks one by its attributes. The pick is recorded with the vehicle
   and passed, signed, with the jobs that follow: a job that does not change an ECU identifies
   each target's variant before its program runs (ADR-325 item 7) and, where it carries a pick
   for that target, takes the picked variant when its patterns match and is refused when they
   do not; without a pick it takes ADR-315's first match, and it is refused when no candidate
   matches. A job changes an ECU at a target when any combination it holds sends to
   that target a request outside the read-only set of item 1, the services of access programs
   aside (ADR-325 item 10): a configuration write, reprogramming, a DTC clear, a routine (which
   can erase or actuate whatever its number), a reset, an input-output control. This is wider
   than the writing job of ADR-257 and ADR-325 item 7, which decides the reprogramming slot and
   is not changed by it. Such a target
   needs its variant identification to match exactly one variant of exactly one ECU definition
   (8.9.1), and the agent can only know that by trying every variant that could be fitted there.
   So for each target it changes the job also holds a guard list: the declaration-only documents
   of every variant, of any ECU definition the tenant may use, whose declared link is on that
   target's bus (item 2's resolved bus) and shares its request or its response identifier (the test ADR-325 item 8 uses
   for two links that reach one ECU), the target's own candidates included. The server compiles
   the list whatever the identification concluded about the vehicle (a model-level fact such as
   the VIN cannot rule out an ECU fitted as a replacement, and neither the selected vehicle nor
   the operator's choice may remove a definition from it) and signs it with the job; `check_job`
   refuses a guard document that is not on the signed list, a list that lacks a candidate of its
   target, and a guard variant whose declared link ADR-324 refuses (amending ADR-318 item 1's
   one document and ADR-325 item 7's job that holds exactly its candidates' documents). A job that
   changes an ECU without a guard list for that target (a bare program, or a document run
   without a server) is refused; a debug build against `sim-vci`, where ADR-247 lets any job
   write, runs the guard only for the targets it holds lists for. The guard runs once the
   target's variant has been identified (after its access program, for a target behind an access
   target), after the job's VIN has matched and with the per-vehicle lock held for a job that
   reads a VIN, and otherwise under the guards the job holds (8.8, ADR-257), and before the
   chosen combination's first instruction, so before anything that changes an ECU is sent: the agent runs variant identification for every variant on the list over the
   target's own link (ADR-325 item 8), sending only the read-only identification requests item 1
   allows. Each of those requests goes to the target's request identifier, and every ECU that
   listens there receives it, as it would receive the job's changes, so each one, like item 2's
   presence request, collects every answer that arrives on each response identifier that a
   listed variant's declared link pairs with the target's request identifier, not only on the
   target's own, for a response window no shorter than the longest P2 time (the time an ECU has
   to start its answer, ADR-235 item 4) that the target's link or any of those declared links
   gives, so a slower ECU's first frame is not missed because the target answered first (amending ADR-235 item 3 for these requests, as item 2
   does for presence); any frame there counts, whether or not a
   multi-frame response would complete. An answer on any of them other than the target's own
   shows a second ECU that would receive the job's changes and refuses the job, and so does a
   second answer on the target's own identifier to one request (a second final response, or a
   response that starts while another is still arriving there; a response-pending answer
   followed by its final response is one answer), which shows two ECUs sharing both
   identifiers; and a job whose
   VCI cannot receive one of those identifiers beside the target's link is refused, so a fitted
   ECU of a listed variant cannot hide behind the target's response filter: it is sent its own
   variant's identification requests and answers them on its own identifier. A guard variant whose declared identifiers differ from the target's link may get no
   answer there, or a negative response its definition does not declare definitive, and so
   leaves the guard inconclusive. The job is refused unless exactly one
   variant on each list matches, it is the variant identified for that target, and every
   identification request of every variant on the list got a positive response that decodes or a
   negative response its ECU definition declares definitive for it (item 3); no answer or an
   undeclared negative response leaves a variant unexcluded and refuses the job, unlike ADR-315's
   selection, which treats it as no match. A refused first run sends nothing after the
   identification requests and the access programs; a refusal on a restart, after a reopening
   of the links or on a handed-over job, once any step of the program has run, ends in on-site
   intervention. This holds whatever the operator picked, so an operator's choice never decides
   which ECU a job changes. The guard runs on every run, a restart's included, together with
   ADR-325 item 7's identification of every target again, after every reopening of the links
   that re-runs identification (ADR-325 item 5), and on a job without a journal as on one with.
   The one exception is ADR-325 item 7's: the reprogramming
   target once the journal has reached its flash plan, which may answer only from its bootloader
   and is validated by ADR-229's hardware identity and state checks instead of by the variant
   match. Even there the check for a second ECU still runs: together with ADR-229's VIN and
   hardware identity reads, before anything that changes an ECU, the agent sends the listed
   variants' identification requests over the target's link and collects answers as above, and
   an answer on a listed response identifier other than the target's, a second answer on the
   target's own identifier to one request (as above), or a VCI that cannot receive one of them,
   ends the job in on-site intervention, so an ECU that came to share the
   target's request identifier while the links were down is still caught; the target's own
   answers to those requests are not evaluated. For it the
   combination record that ADR-325 item 7 commits (or, for a job that commits none, such as a
   single target without an access target, a guard record the journal commits at the same
   point, holding as that record does the hash of the document whose program runs and the
   variant identified), now only after the guard has passed on every
   target the job changes and before the first instruction, also records that the guard passed
   and the hashes of the target's guard list (amending ADR-318 item 4 and ADR-325 item 7's
   record; for such a restart this also ends ADR-318's consequence that a replacement document
   goes undetected, which still holds before the flash plan), and a restart once the journal has reached the flash plan (inside it or past its
   end) that carries a different guard list for that
   target, whose program's document or identified variant differs from the record, or whose
   record (combination or guard) lacks the pass or is missing, ends in on-site intervention as a
   variant that differs from the recorded combination does. The record is part of the checkpoint summary a handover carries, so a handed-over
   restart is held to the same list.
8. **Manual paths.** The operator can choose a vehicle definition by hand (an unregistered or
   misidentified vehicle), and the plan then holds that definition's entries and the ECU
   definitions they name, without candidates, so a fact that contradicts the chosen definition
   (the reason it was chosen by hand) cannot end the job early; or ask for access to every ECU,
   for which the plan holds every ECU definition the tenant may use, also without candidates. A
   definition chosen by hand keeps its VIN matching rules in the plan without narrowing anything,
   so the VIN is still read from the sources they name (item 5), reported, and used to promote to
   the per-vehicle lock (8.8). In both, items 4 and 5 read every definition in the plan with no
   narrowing, and each entry's state is reported as item 6 says. Both are recorded as operator
   choices. Neither changes item 7: a job that changes an ECU is guarded whichever way the
   vehicle was chosen.

## Consequences

- ADR-326 item 5 changes: the agent runs identification from a plan and guards jobs that change
  an ECU, so its variant identification and its runtime change, against that item's last
  sentence and the Consequence that a vehicle definition adds no runtime path. The agent still
  holds no vehicle definition, and the server remains the authority for the selected definition
  and for authorization.
- ADR-326 item 3 changes: an entry whose alternatives both match is ambiguous instead of taking
  the first, and an entry becomes an unrecognized variant only when every alternative using its
  endpoint has been read without a match. ADR-326 item 2's VIN rule gains its source.
- An identification plan can be large for a tenant with many models or for access to every ECU;
  its documents are fetched lazily by hash, but each read costs bus time, so the worst-case
  choice in item 4 matters most on large plans.
- Item 7 tightens every job that changes an ECU: before it, such a job carried one variant's
  document and nothing checked that no other variant matched. Variant patterns that overlap now
  block those jobs until they are made exact, as do alternative ECU definitions on one bus whose
  patterns overlap or whose identifiers only partly match the target's (their variants get no
  answer on the target's link); a job's payload grows by its guard lists.
- The guard sees a second ECU on the target's request identifier only by its answer on its own
  response identifier, so each guard request waits out its whole response window, and an ECU
  there that answers none of the identification requests, its own variant's included, stays
  unseen, and so does one that answers on a response identifier no listed variant declares (an
  unknown device, or one of an ECU definition the tenant may not use), since without a
  declaration an answer there cannot be told apart from the bus's other traffic. Two ECUs that
  share both of the target's identifiers are seen only when their answers arrive as separate
  frames; identical answers sent at the same instant merge on the bus and look like one. A job refused because its VCI cannot receive those identifiers beside the target's
  link needs another VCI. These are accepted.
- Until ECU definitions declare definitive negative responses, every negative response leaves a
  read inconclusive, so a guard is refused whenever any variant on its list (the target's own
  other patterns included) gets a negative response or no answer; the guard can be used in
  practice only once that declaration exists.
- An access target's variant is chosen by ADR-315's first match and is not guarded, and its
  access program (session control and security access) is sent before any guard runs; a target
  behind an access target is guarded only after it. A guard's expected silences behind an
  access target wait out ADR-325 item 12's hold, which lengthens the guard there. Both are
  accepted.
- Identification through an access target (ADR-325 item 5) is not covered: an identification job
  runs no access program, so an ECU behind a locked gateway is reported as not answering.
  Identifying through an unlocked gateway is a later decision.
- Follow-up work (the agent's identification job, the server's plan, guard lists and
  re-evaluation, variant attributes, VIN sources and definitive negative responses in the CSV
  format and the IR, the read-only check of identification services, the guard) is in the
  backlog.
