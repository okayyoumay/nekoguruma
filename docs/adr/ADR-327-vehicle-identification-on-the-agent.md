# ADR-327: Vehicle Identification Runs on the Agent

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `agent` (an identification job, write jobs' variant check), `server` (identification plans, re-evaluation of
the reported facts), `diag-frontend` and `diag-ir` (operator-facing variant attributes), design
8.8.2, 8.9.1; ADR-235 item 8, ADR-315 (ambiguous variant matches), ADR-318 items 1 and 4, ADR-324 items 1 and 2, ADR-326 items 3 and 5; `docs/glossary.md`

## Context

ADR-326 describes vehicles by vehicle definitions: identification values with their matching
rules, and a topology of ECU entries whose alternatives are ECU definitions. It leaves the
server to identify a vehicle from values a read-only job reports, and says the agent does not
use vehicle definitions.

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
   candidate set that ADR-326 item 5 evaluates a role's scope against. The plan holds, per candidate, its identifier, its
   identification values and their matching rules, and its ECU entries (role, alternative ECU
   definitions, optional flag, gateway entry); and the documents of every variant of every ECU
   definition the candidates name, referenced by hash as other job documents are (ADR-318), so
   variant identification and field decoding work as in any job. The agent still fetches no
   vehicle definition (amending ADR-326 item 5): the plan is all it reads, and the server can
   prefetch it with the job for offline work. The job holds the per-VCI lock and promotes to the
   per-vehicle lock when it reads a VIN (8.8); it sends only reads and presence requests.
   Every service a variant pattern or a VIN source names must be in ADR-235 item 8's read-only
   set: a document whose identification would send anything else is rejected when it is
   ingested, and a plan or job that carries one is refused before any link opens, so neither
   an identification job nor item 7's check can itself be the first request that changes the
   ECU; the host's check of every request (ADR-235 item 8) stays the second checkpoint.
2. **Presence first.** For each distinct link of the ECU definitions in the plan whose protocol
   declares a functional identifier, the agent sends one presence request functionally (for UDS,
   TesterPresent with a response requested) and records each physical response identifier that
   answered. An
   endpoint is a physical address: the bus (protocol family, pins and baud rate, which tell
   physical networks apart) and the physical request and response identifiers; definitions
   whose links differ only in timing or other ComParams that do not distinguish the network
   share an endpoint, since they can reach the same ECU. A functional answer shows only the bus
   and the response identifier, not which physical request identifier the ECU listens on, so a
   presence answer is recorded as that pair and counts for every endpoint with that bus and
   response identifier. Presence is keyed this way, not by ECU definition, because alternatives such as two suppliers' ECUs for one role
   often share an endpoint, and an answer says only that one of the definitions that could give it is
   fitted, until a read tells which. An answer is evidence of presence; silence is not
   evidence of absence, since an ECU behind a gateway, a sleeping ECU or one on a link without
   functional addressing may not answer. This is the one functional request the agent sends,
   and it uses the functional identifier the ECU definition's protocol declares, which ADR-324
   item 2 leaves unused for a document job's link; that link still sends only physical
   requests. ADR-235 item 8's read-only policy forbids TesterPresent because it keeps
   a session alive that the job may not own; the presence request is excepted for an
   identification job only. It is sent once per link, never repeated as a keep-alive. Like any
   request, it restarts the S3 timer of an ECU that is already in a non-default session opened
   by someone else, but the reads every read-only job sends do the same (ISO 14229-2 (2021)
   9.5 restarts the timer on any request from the client that opened the session, and a
   tester on the same identifiers cannot be told apart from that client; `sim-ecu` models
   it so), so the exception adds no new effect on an
   unowned session; what ADR-235 keeps forbidden is a TesterPresent sent to hold a session
   open, and every other TesterPresent stays forbidden to read-only jobs.
3. **Narrowing.** A candidate is dropped only when a fact contradicts it: a presence answer
   (bus and response identifier) that no ECU definition in any of its entries could have given, an identified variant whose ECU definition
   none of its entries allows (counted only once every other definition that uses the same
   endpoint and is still named by a remaining candidate has been read conclusively on that endpoint, since an ECU can match the patterns of two alternative
   definitions; a sibling that is out of reach or inconclusive, item 5, has not been ruled out, so the candidates that need it stay and, if more than one candidate is
   left at the end, the operator chooses; when several match, all of them stay possible
   and the operator chooses, as for variants in item 7, amending ADR-326 item 3's first match), or an identification value whose matching rule (a VIN position
   range, or a read field) gives a value outside the candidate's. Conclusiveness is judged per
   definition and endpoint, over the variants whose declared links are on that endpoint: a read
   there is conclusive when every identification request of those variants got either a positive response that decodes or a
   negative response that the ECU definition declares, for that request, as proof that the ECU
   cannot report what its patterns expect. No negative response proves that by default: the
   same code (requestOutOfRange, for example) can mean an identifier the ECU lacks or one that
   is unavailable in the current session or state, so only the ECU definition's author can say
   which codes are definitive for that ECU. A conclusive read on an endpoint
   whose variants there all fail to match rules out only that definition on that endpoint; a read with no variant matching
   in which some request got no answer or an undeclared negative response is inconclusive and rules out nothing, since ADR-315 counts those
   entries as failed; the
   others sharing the endpoint stay possible, and the endpoint becomes an unrecognized variant
   (ADR-326 item 3) only when every definition using it has been read without a match. A
   missing answer never drops a candidate. An ECU definition that no remaining candidate names,
   or that a read has ruled out, is no longer read.
4. **The next ECU.** Among the ECU definitions still named by a remaining candidate (in a plan
   without candidates, item 8, every definition in the plan) and not yet read or out of reach,
   the agent reads the one whose outcome leaves the fewest candidates in the worst case,
   counting as outcomes the distinct sets of candidates that a possible response would leave:
   each variant it may report, combined with the values the candidates' matching rules expect
   from the fields read with it, grouped by the candidates the response satisfies (two
   candidates whose expected values overlap, such as one prefix of the other, stay in one
   outcome), and a conclusive match of no variant, likewise grouped by the
   candidates the values of its decoded rule fields satisfy. No answer and an inconclusive read are left out of the count: they narrow nothing
   (item 3), so they would leave every candidate
   for every definition and erase the differences between them; how likely an answer is enters
   only through the tie-breakers. Ties go to the one named by
   more remaining candidates, then to one with more of its endpoints matched by a presence answer (item 2), then to the lowest
   identifier, so the order is deterministic. A read is the definition's variant identification
   (8.9.1) and the fields the candidates' matching rules read from it, each variant
   tested over the link it declares (ADR-324 item 1, which gives each variant its own declared
   link): the variants are grouped by declared link and each group is identified on its link,
   so the job opens one link per distinct link it reads (amending ADR-324 item 1's single link
   per job for this job kind). The endpoints of a definition are those of its variants' links. Each ECU definition is
   read at most once per job.
5. **End.** The loop ends when every ECU definition still named and not ruled out has been
   read or is out of reach, or when no candidate remains; a plan without candidates (item 8)
   ends when every definition in it has been read or is out of reach. Out of reach is
   established only by a physical read, per definition: a definition whose identification
   requests all go unanswered is out of reach, and one read inconclusively (item 3) is
   inconclusive; both count as tried for the end of the loop and
   keep the candidates that need them. Alternatives that share an endpoint may use
   different identification requests, so each one is still tried; the endpoint itself counts
   as out of reach only when every definition using it that is still in play has been tried
   without an answer. Silence to the presence request alone never puts a definition out of
   reach. When a
   candidate declares a VIN matching rule, the VIN is read first: the distinct VIN sources the
   candidates' rules name (a service and its link) are tried in turn, in a fixed order, until
   one gives a VIN that decodes, and a VIN no source gives leaves every VIN rule unevaluated; when none does, identification reads no VIN (the data author's choice).
6. **The agent reports facts; the server decides.** The job reports the VIN when read, the
   presence answers, each read's decoded identification fields and matching variants,
   the outcome of each of its identification requests (decoded positive response, the negative
   response code, or no answer, so the server can tell a conclusive read from an inconclusive
   one by item 3), and the
   entry states of ADR-326 item 3. The server recomputes the candidates from these facts with
   the same rules, so the agent's narrowing only chooses what to read. One candidate is selected;
   several go to the operator, as ADR-326 item 2 says, with ADR-326 item 5's rule that operator
   choices never widen what a role permits.
7. **Ambiguous variants and operator attributes.** A variant may carry operator-facing
   attributes, labels with values (transmission: manual), that say nothing to the ECU and serve
   only this choice. When more than one variant of an ECU definition matches in an
   identification job, the agent reports all of them instead of the first one ADR-315 selects,
   and the operator picks one by its attributes. The picked variant can be used for reads and
   other jobs that do not write; such a job checks, before its procedure runs, that its
   variant's patterns match the ECU, and is refused when they do not. A job that can change
   the ECU (a write job in this ADR: one whose procedure writes configuration values,
   reprograms, clears DTCs, runs a routine, which can erase or actuate whatever its number,
   or sends any other request outside ADR-235 item 8's read-only set) needs its variant identification to match exactly one
   variant of exactly one ECU definition (8.9.1), and the agent can only know that by trying
   every variant that could be fitted there. So a write job carries, beside its own variant's
   document, the documents of every variant, of any ECU definition the tenant may use, whose
   declared link resolves to the job's endpoint, its own definition's other variants there
   included; variants of those definitions on other endpoints are not carried, since they
   could not be fitted at this one. This holds whatever the identification concluded about
   the vehicle: a model-level fact such as the VIN cannot rule out an ECU fitted as a
   replacement, and neither the selected vehicle nor the operator's choice may remove a
   definition from the check (amending ADR-318 item 1, under
   which a job carries one variant's document). Before anything that writes is sent the agent
   runs variant identification over all of them, each variant over the link it declares
   (links on the same endpoint that differ only in timing ComParams), one link per distinct
   link, opened in turn and closed again (amending ADR-324 item 1, under which a document job opens
   only its own variant's link), and then opens the job's own declared link for the procedure, and the job is refused unless exactly one
   variant of one definition matches, it is the job's own, and every request of every carried variant
   was answered conclusively in item 3's sense (no
   answer, or a negative response the ECU definition does not declare definitive, leaves a
   variant unexcluded, which makes the check inconclusive and refuses the
   write, unlike ADR-315's selection, which treats it as no match). The check sends only the
   read-only identification requests item 1 allows. This holds whatever the operator picked,
   so an operator's choice never decides a write's target. The check guards a job's first run:
   the agent journals that it passed before the first request that changes the ECU, together
   with the hashes of the program and of every document the check covered, the job's own
   document's hash and its matched variant recorded apart from the others (the journal
   otherwise identifies neither, ADR-318 item 4, which this amends for these jobs), and a
   restart of an interrupted reprogramming job whose journal records it with the same hashes,
   the same document as the job's own included, as the restart carries does not repeat it,
   since an erased application may no longer answer the identification requests; the restart
   identifies the ECU by the VIN and hardware part number journaled before the erase, under
   ADR-229 item 2's rules, instead. Every other restart runs the check again: one whose
   journal does not record a passed check has changed nothing yet, and a job that does not
   erase the application (a configuration write, a DTC clear, a routine) journals no such
   identity, while its ECU can still answer, so the check is the only way to confirm the
   restart reaches the same ECU; one whose carried program or documents differ from the recorded
   hashes is refused, since the ECU may already be changed and the check could not be rerun.
8. **Manual paths.** The operator can choose a vehicle definition by hand (an unregistered or
   misidentified vehicle), and the plan then holds that definition's entries and the ECU
   definitions they name, without candidates, so a fact that contradicts the chosen definition
   (the reason it was chosen by hand) cannot end the job early; or ask for access to every ECU,
   for which the plan holds every ECU definition the tenant may use, also without candidates.
   A definition chosen by hand keeps its VIN matching
   rules in the plan without narrowing anything, so the VIN is still read from the sources they
   name (item 5), reported, and used to promote to the per-vehicle lock (8.8). In both, items 4
   and 5 read every definition in the plan with no narrowing, and each entry's
   state is reported as ADR-326 item 3 gives it. Both are recorded as operator choices.

## Consequences

- ADR-326 item 5 changes in one point: the agent runs identification from a plan. It still holds
  no vehicle definition, and the server remains the authority for the selected definition and
  for authorization.
- ADR-326 item 3 changes in one point: when the ECU at an entry matches the patterns of more than
  one of its alternatives, the first is no longer taken as fitted; all matching alternatives stay
  possible and the operator chooses (item 3).
- An identification plan can be large for a tenant with many models or for access to every ECU;
  its documents are fetched lazily by hash, but each read costs bus time, so the worst-case
  choice in item 4 matters most on large plans.
- Item 7 tightens writes: before it, a write job carried one variant's document and nothing
  checked that no other variant of its ECU matched. Variant patterns that overlap now block
  writes until they are made exact, as do alternative ECU definitions on one endpoint whose
  patterns overlap; a write job's payload grows by the documents of every variant that could be
  fitted at its endpoint.
- Gateway unlocking before identification is not covered: an ECU behind a locked gateway is
  reported as not answering. Identifying through an unlocked gateway is a later decision, with
  the multi-ECU procedure design.
- Follow-up work (the agent's identification job, the server's plan and re-evaluation, variant
  attributes and definitive negative responses in the CSV format and the IR, the read-only check of identification services, the write refusal on ambiguous variants) is in the
  backlog.
