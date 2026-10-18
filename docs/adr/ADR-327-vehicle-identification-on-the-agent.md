# ADR-327: Vehicle Identification Runs on the Agent

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `agent` (an identification job, write jobs' variant check), `server` (identification plans, re-evaluation of
the reported facts), `diag-frontend` and `diag-ir` (operator-facing variant attributes), design
8.8.2, 8.9.1; ADR-315 (ambiguous variant matches), ADR-318 item 1, ADR-324 item 2, ADR-326 items 3 and 5; `docs/glossary.md`

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
   requests.
3. **Narrowing.** A candidate is dropped only when a fact contradicts it: a presence answer
   (bus and response identifier) that no ECU definition in any of its entries could have given, an identified variant whose ECU definition
   none of its entries allows (counted only once every other definition that uses the same
   endpoint and is still named by a remaining candidate has been read conclusively, since an ECU can match the patterns of two alternative
   definitions; a sibling that is out of reach or inconclusive, item 5, has not been ruled out, so the candidates that need it stay and, if more than one candidate is
   left at the end, the operator chooses; when several match, all of them stay possible
   and the operator chooses, as for variants in item 7, amending ADR-326 item 3's first match), or an identification value whose matching rule (a VIN position
   range, or a read field) gives a value outside the candidate's. A read is conclusive when every
   identification request of the definition got either a positive response that decodes or a
   negative response that the ECU definition declares, for that request, as proof that the ECU
   cannot report what its patterns expect. No negative response proves that by default: the
   same code (requestOutOfRange, for example) can mean an identifier the ECU lacks or one that
   is unavailable in the current session or state, so only the ECU definition's author can say
   which codes are definitive for that ECU. A conclusive read whose
   variants all fail to match rules out only that definition; a read with no variant matching
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
   outcome), and a conclusive match of no variant. No answer and an inconclusive read are left out of the count: they narrow nothing
   (item 3), so they would leave every candidate
   for every definition and erase the differences between them; how likely an answer is enters
   only through the tie-breakers. Ties go to the one named by
   more remaining candidates, then to one whose endpoint answered the presence request, then to the lowest
   identifier, so the order is deterministic. A read is the definition's variant identification
   (8.9.1) and the fields the candidates' matching rules read from it. Each ECU definition is
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
   candidate declares a VIN matching rule, the VIN is read first, from the source the plan
   names; when none does, identification reads no VIN (the data author's choice).
6. **The agent reports facts; the server decides.** The job reports the VIN when read, the
   presence answers, each read's decoded identification fields and matching variants, and the
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
   variant's patterns match the ECU, and is refused when they do not. A job that writes
   configuration values or reprograms an ECU needs its variant identification to match exactly one
   variant of exactly one ECU definition (8.9.1), and the agent can only know that by trying
   every variant that could be fitted there. So a write job carries, beside its own variant's
   document, the documents of every variant of every ECU definition that uses the same endpoint
   among those the vehicle's identification considered (the entries of every candidate still
   left when the identification ended, not only the selected one, so the operator's choice of
   a vehicle cannot remove a definition from the check; or, when the vehicle was identified by hand or without a definition, every ECU
   definition the tenant may use), its own definition included (amending ADR-318 item 1, under
   which a job carries one variant's document). Before anything that writes is sent the agent
   runs variant identification over all of them, and the job is refused unless exactly one
   variant of one definition matches, it is the job's own, and the read of every carried definition
   was conclusive in item 3's sense, the job's own definition's other variants included (no
   answer, or a negative response the ECU definition does not declare definitive, leaves a
   variant unexcluded, which makes the check inconclusive and refuses the
   write, unlike ADR-315's selection, which treats it as no match). This holds whatever the operator picked,
   so an operator's choice never decides a write's target.
8. **Manual paths.** The operator can choose a vehicle definition by hand (an unregistered or
   misidentified vehicle), and the plan then holds that definition's entries and the ECU
   definitions they name, without candidates, so a fact that contradicts the chosen definition
   (the reason it was chosen by hand) cannot end the job early; or ask for access to every ECU,
   for which the plan holds every ECU definition the tenant may use, also without candidates.
   In both, items 4 and 5 read every definition in the plan with no narrowing, and each entry's
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
  attributes and definitive negative responses in the CSV format and the IR, the write refusal on ambiguous variants) is in the
  backlog.
