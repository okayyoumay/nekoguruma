# ADR-395: The Identification Plan Format, Its Checks and the Endpoint Key

**Date:** 2026-10-30
**Status:** Accepted (item 6's halving trigger made concrete, and the presence request given its form, by ADR-396; item 3's checks gain an entry that lists an alternative twice, by ADR-401)
**Affects:** `diag-ir` (`plan`), `agent` (`identification`), `crates/agent/docs/ngr-agent.md`,
`docs/glossary.md`, design 8.8.2; ADR-318, ADR-319, ADR-324, ADR-326, ADR-327

## Context

ADR-327 has the agent run vehicle identification from an identification plan that the server
compiles and signs with the job, and says what the plan holds, but not how it is encoded, what
the agent checks before a link opens, or how it derives endpoints and presence groups from the
declared links. The server that compiles plans is a placeholder, and the work on vehicle
definitions there takes its format from here. The agent's identification job is built in
steps: this ADR records the format, the checks and the derivation (implemented now, sending
nothing on a bus) and the rules for the presence request and the receive table that the next
step implements.

## Decision

1. **The plan is a postcard struct in `diag_ir::plan`.** `IdentificationPlan` has its own
   `schema_version` as its first field (`PLAN_SCHEMA_VERSION`, independent of
   `IR_SCHEMA_VERSION`), as `Program` has, and `decode` reads that version alone first, so a plan
   of another version is refused as such. It holds a `selection`, the candidates, and the
   ECU definitions with their documents. A candidate holds its id, its identification values, its
   VIN rules and field rules (each with its source), and its entries (role, optional flag,
   gateway role, alternative definition ids). A source is a document hash, a service id and a
   field id, as ADR-327 item 5 has it (service and field ids are local to a document, ADR-318).
   `selection` records how the vehicle was chosen, which ADR-327 item 8 has the job report:
   `Candidates` (the server's candidates, the only selection that narrows), `ChosenDefinition`
   (a definition the operator chose by hand, held as the plan's only candidate, which supplies
   its entries and VIN rules and narrows nothing) or `EveryEcu` (access to every ECU, with
   definitions and no candidate). The plan is refused when a chosen-definition plan does not hold
   exactly one candidate or an every-ECU plan holds any. Item 8 says both manual plans hold
   no candidates; this format carries a chosen definition's entries and VIN rules as the one
   candidate instead, since candidates are where the plan keeps them, and that candidate narrows
   nothing, so item 8's outcome holds (ADR-327 is annotated). The
   plan holds no ECU declarations: a definition lists its documents by SHA-256, and the
   documents come beside the plan.
2. **Signature coverage.** The signature that the server-connected agent verifies later (the
   shared crypto crate is a stub until then) covers the SHA-256 of the plan's bytes together with
   the job instruction. The plan names every document by its SHA-256, so the signature covers
   the documents too, as ADR-318 and ADR-325 bind documents by hash. The agent hashes each
   supplied document and compares it with the name the plan gives it.
3. **Refused before any link opens.** `load_plan` and `check_plan` (`agent::identification`)
   are the only gate before a link opens. The plan is refused when:
   - its version is unknown, its bytes do not parse or leave bytes over;
   - a selection's candidate count is not the one item 1 gives it, or a `Candidates` plan holds
     none;
   - a candidate, definition or role id is empty or used twice (a role within its candidate), an
     entry has no alternative or lists one twice (ADR-401), a definition lists no document, or a gateway is its own entry or
     names no role of the candidate. Two values of one component are allowed (design 8.8.2);
   - an entry names a definition the plan lacks, or a rule source names a document no
     definition lists, or a document is listed under two definitions (listed twice under one is
     harmless);
   - a VIN rule's positions (from 1) are not within 1 to 17 with the start not after the end, or
     its expected text is not as long as the range;
   - a document the plan names is not supplied, its bytes do not hash to its name, or it does not
     load;
   - a service that a variant pattern or a rule source names is not declared or its request is
     outside the read-only set of the variant's application layer (ADR-327 item 1). A variant
     whose protocols name no layer with a read-only list (none declared, or another layer) is
     decided to judge nothing read-only, because `ApplicationLayer::of_job` gives it no list:
     a pattern or source naming a service there is refused as such, not reported as a refused
     link, so the refusal does not depend on the link;
   - a rule source names a field the service's positive response lacks;
   - one resource (protocol, bus type and so the same pins, ADR-319) is declared at two baud
     rates by the plan's links, since the job opens one resource at one rate;
   - a functional identifier is wider than the 11-bit identifiers the link supports
     (`LinkConfig::validate`) or equals a physical request or response identifier of a link on
     the same bus;
   - declared links in one presence group (item 5) differ in a ComParam (item 6).

   A declared link that ADR-324 refuses does not refuse the plan: ADR-327 item 3 leaves such a
   variant's definition inconclusive when it is read, so the check reports the document and the
   reason, the document gives no endpoint, and the read treats it as unreadable.
4. **Bus key and endpoint key.** The bus key is what ADR-327 item 2 calls the bus: the resource
   the agent resolves for the link, which is its protocol short name and bus type (the pins
   follow from the resource, ADR-319, so they are not a separate part of the key), with the baud
   rate, so two spellings of one stack that ADR-324 item 2 accepts are one bus. An endpoint key
   is a bus with the physical request and response identifiers. Documents whose links give one
   endpoint key are one endpoint entry that counts for each of them. Two endpoints may share a
   response identifier with different request identifiers (ADR-327 item 2 lets a presence answer
   count for both). The receive table of the next step has one entry per distinct (request,
   response) pair, so they are two entries; if the VCI refuses a table that repeats a response
   identifier, item 6's halving can separate the pairs, and a single pair still refused refuses
   the job. This is the conservative reading: no pair is merged.
5. **Presence group and window (decided here, implemented by the presence request).** A presence
   group is a bus and a functional identifier that a declared protocol gives; a bus on which no
   declared link has one gets no group and is reported. The presence request uses the API's own
   window: the presence link's `CP_P2Max` is the longest over the declared links of the group,
   which is the window ADR-327 item 2 asks for, and the request collects every answer
   (`num_receive_cycles` of -2, ISO 22900-2:2022 8.2.6.2.2 Table 6). That clause has the API
   restart its receive timer for each matching answer; the J2534 worker does so (the RPC guide
   describes its window), and the native D-PDU worker leaves it to the vendor library. ISO
   14229-2:2021 10.2.1 has a client restart its own response timer from its default reload value
   at each response to a functional request. The communication log judges where the window ended.
6. **Limits and ComParams (decided here; the check is implemented, the halving by the presence
   request).**
   - Declared links in one presence group must agree in every ComParam other than a timing,
     compared by name ignoring case, as the workers resolve names (ADR-324 item 3). A ComParam
     declared on one link and missing on another is a difference. A bus without a group is not
     compared, and groups with different functional identifiers on one bus are not compared with
     each other.
   - **Timings** are an explicit list of identifiers, the agent's own classification of the
     timing ComParams this repository deals with: `CP_P2Max`, `CP_P2Star`,
     `CP_RC78CompletionTimeout`, `CP_P2Min`, `CP_P3Func`, and the transport timings `CP_As`,
     `CP_Ar`, `CP_Bs`, `CP_Br`, `CP_Cr` and `CP_Cs`. They are left out of the comparison, and
     the presence link takes the largest value of each across the group's links. A name not on
     the list counts as a non-timing ComParam and a difference refuses the plan, the safe side.
   - **Why the largest is acceptable.** For a timeout or a window (`CP_P2Max`, `CP_P2Star`,
     `CP_RC78CompletionTimeout`, `CP_As`, `CP_Ar`, `CP_Bs`, `CP_Cr`) a larger value only makes
     the tester wait longer for an ECU. For the tester's own minimum gaps and send delays
     (`CP_P2Min`, `CP_P3Func`, `CP_Br`, `CP_Cs`) a larger value only makes the tester slower, so
     no ECU's timing is violated by it; no listed timing is one where a larger value would send
     sooner than an ECU allows. A ComParam added to the list must pass the same test.
   - The values beyond `CP_P2Max` are computed and reported now (`p2_star_ms`,
     `rc78_completion_ms`, `timings`); the presence request applies them.
   - When the VCI refuses a receive table of every declared (request, response) pair on the bus,
     the table is halved in declaration order and each part is opened again until the VCI
     accepts it. Each part gets a presence request on a link of its own, one link open at a time,
     under item 5's window. A part of a single pair that is still refused refuses the job.
7. **The report.** `IdentificationReport` holds the selection, the candidate ids, the endpoint
   entries, the presence groups with the presence link's P2, the buses without a presence group,
   the documents whose link is refused, and a presence section that stays empty until the
   presence request exists. Only `CheckedPlan::report` builds it, so no other code writes the
   endpoint and presence-group lists.
8. **Out of scope here.** No bus traffic, no CLI or job kind, and no compilation from a
   vehicle definition. `check_job`, `run_program` and the read-only policy are unchanged, so
   TesterPresent stays refused to every job until the presence path exists.

## Consequences

- The server's plan compiler and the agent share one format and its checks, and a test plan is
  built by hand until the compiler exists.
- A plan carries a version of its own, so the vehicle definitions can change it without a
  declaration-schema bump.
- Two documents of one endpoint appear once in the endpoint list; the presence answers
  (a bus and a response identifier) later count for every endpoint with that pair.
- A presence group whose links disagree in a non-timing ComParam cannot be identified until
  the definitions agree; this is stricter than ADR-324 alone, which judges one link at a time.
- A resource declared at two baud rates cannot be identified in one job; the plan is refused
  rather than split.
- The signature check, the presence request, the collecting mode, the halving, the job kind and
  its CLI are follow-up work, in the backlog.
