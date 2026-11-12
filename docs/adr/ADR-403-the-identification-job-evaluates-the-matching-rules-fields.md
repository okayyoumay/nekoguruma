# ADR-403: The Identification Job Evaluates the Matching Rules' Fields

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `agent` (`identification`: `check_plan`, `PlanRefused::FieldSource`,
`group_rule_fields`, `read_definition_at`, `evaluate_rule_fields`, `contradictions`,
`outcome_sets`, `rule_classes`, `mark_unread`, `evaluate_entries`, the report), `diag-ir`
(`decode::expected_matches`, `decode::expected_value`, `decode::ExpectedKind`,
`identify::identification_request_of`, `plan::validate`), `crates/agent/docs/ngr-agent.md`,
design 8.8.2, `docs/glossary.md`; ADR-306, ADR-326 item 2, ADR-327 items 3, 4 and 6, ADR-395
item 3, ADR-397, ADR-399 item 2 (the source check), ADR-401 items 2, 4, 8 and 11, ADR-402 items
2, 3, 5 and 7

## Context

ADR-327 item 4 has a read be the definition's variant identification and the fields the
candidates' matching rules read from it: each rule's source is a service and a field of one
variant document, read over that variant's link, a component's value is established only when
every rule source on that service and link decodes the response to the same value, and the values
the rules expect are counted as outcomes of the read. Item 3 has an established value outside a
candidate's own contradict it, and item 6 has each rule field's established value, or that it
stayed unevaluated, reported. ADR-399 read the VIN, ADR-401 the definitions and ADR-402 narrowed
on them and built the outcome enumeration with variant outcomes only, where the worst case cannot
tell definitions apart; it left the rule fields as the plug point and the mixed outcome (identified
or established at one endpoint, inconclusive or unevaluated at another) open. Nothing read a rule
field yet.

## Decision

1. **A rule field is read with its definition.** A field rule's source is a declaration of the
   request its service builds (`identification_request_of`, the function
   `identification_requests` builds each of its requests with), in the link group that holds its
   document, at that document's endpoint. It is flagged `pattern: false` unless a variant pattern
   names the same (document, service). Each distinct request is sent once per group, with the
   others, on the same link, and a rule alone opens nothing beyond what its definition's read
   opens (ADR-401 items 1 and 2). A rule on a definition never read (not in play, or bounds that
   ran out) stays unevaluated.
2. **Established on agreement.** One rule field is a (definition, endpoint, link group, request,
   component): the sources of every candidate's rules on it decode the one response, and the
   component's value is established only when every source decoded and all gave the same
   physical value; a text and bytes of equal content differ (another layout decoding to another
   value). Rules for different components on one request are distinct rule fields, judged apart.
   Two definitions reading one request at one endpoint are two rule fields and two sends, as
   ADR-401's Consequences accept for the VIN; two link groups of one definition at one endpoint
   reading one request are two rule fields, told apart in the report and in `FieldRules` by the
   group's index within the read. Every planned source takes part in establishing the field,
   whichever candidate's rule names it and whether or not that candidate is dropped: this is the
   guard ADR-327 item 4 gives against another layout, and a dropped candidate's source is not
   left out.
3. **Compared as a pattern.** A rule is `Matched` when `expected_matches` (the comparison
   `pattern_matches` makes, ADR-306: bytes and text by prefix, integers by number, a text table by
   its internal value) holds for the established value, `NotMatched` otherwise, and `Unevaluated`
   when the field is not established. Only the established value is compared, so a rule on a
   variant's layout that did not match can match; the agreement rule is the guard ADR-327 item 4
   gives, and the author's defence is one source per variant.
4. **The contradicting fact is per component and rule field.** A candidate with rules on an
   established field none of which matched is contradicted (`FieldRules`, naming the component,
   definition, endpoint and request). A definition lists several values of a component as several
   rules on the field, so one matching rule keeps the candidate (ADR-402 item 3's reason). Another
   field of the same component, matched or not, neither saves nor drops it: each field is a
   witness of its own. An unestablished field drops nothing. `after` is the definition whose read
   established the field. This extends ADR-402 item 2's facts.
5. **Outcomes.** `outcome_sets` gains, per rule field of the definition at an endpoint, a list of
   alternatives (`rule_classes`): one class per distinct value the remaining candidates' rules
   expect, built from `expected` by the field's kind (`expected_value`), so one expected value
   that is a prefix of another lands in one class with it, and one class for the value no rule
   matches; each class is judged by `contradictions`, as the run is, and `fold_outcomes` combines
   the lists with the endpoints' unchanged. The fold stays exact because a `FieldRules` drop
   depends on its own field only. Unevaluated is not an outcome, at any endpoint, as an
   inconclusive read is not: the mixed outcome is not counted, because it would weaken a rule
   field that separates every candidate to the remaining count, which is why ADR-327 item 4
   leaves inconclusive reads out. Exactness rests on the plan check of item 6: a rule field whose
   sources could never agree is refused, so every rule field in a plan can be established and
   its classes are real outcomes. This settles the question ADR-402 item 5 left open. A definition
   whose rule field separates the candidates is therefore read before one that cannot drop
   anything.
6. **The plan check refuses a rule that could never be judged,** before any link opens
   (`PlanRefused::FieldSource`, after ADR-399 item 2's `VinSource`): a source whose request needs
   values; whose field ADR-306 item 5 says no pattern can compare (a float, a conversion, a nested
   layout, `ExpectedKind::of_field`); or whose `expected` could never match
   (`ExpectedKind::never_matches`, every case of ADR-306 item 5: empty; over eight bytes for an
   integer; a number outside what the field's fixed width holds, a BCD field holding a decimal
   digit per four bits; longer than a fixed-length bytes or text field can be, text reaching 2n
   bytes of UTF-8 for n ASCII bytes and 3n/2 for UTF-16; text that is not UTF-8). It also
   refuses a rule field whose sources cannot decode to one value: an integer and a text or bytes
   source, a text and a bytes source, or fields of one data kind at two fixed lengths. `diag_ir::plan::validate` refuses a field rule whose source is a
   document of a definition no entry of the candidate lists (the frontend's own rule, ADR-395
   item 3's list grows). A refused link is reported, not refused: the rule field has a null
   endpoint and `LinkRefused` sources.
7. **Conclusiveness and `has_request` count the pattern declarations only** (ADR-327 item 3;
   ADR-401 item 4 amended): a refused or undecodable rule request leaves the variant read
   conclusive. `answered` counts every declaration, since an ECU that answered the rule's
   request is there (ADR-326 item 3's "never hidden as absent"). The entry states leave a
   definition out when it has no pattern declaration, as before.
8. **A definition with no pattern that a field rule names is read** for the rule: its link opens
   and the rule's request is sent, the read is never conclusive and never matches, and its entries
   are judged as if the definition had no declaration. This amends ADR-401 item 8, whose
   pattern-less definition opened no link.
9. **The report** gains `rule_fields` (per field: definition, endpoint, request, component,
   sources with their outcomes, the established value and the rules on it with their outcomes),
   `component_values` (the distinct component values the matched VIN and field rules of the
   candidates remaining at the end give, VIN rules first, reported even for a plan that does not
   narrow; a dropped candidate's matched rule gives none), `Contradiction::FieldRules`, and
   a `pattern` flag on each declaration; `VinRuleOutcome` is renamed `RuleOutcome` (the JSON is
   unchanged). An interrupted phase keeps the established fields and their drops; sources not
   sent stay `NotRead` and those of definitions not in play are `NotInPlay`.

## Consequences

- A rule field that separates the candidates is read before a definition that cannot drop
  anything, so the count now orders reads; among definitions without rule fields the tie-breaks
  still decide.
- A plan can be refused for a rule a converted definition carries until the frontend refuses
  such rules at conversion (a backlog item); `expected` is raw bytes there, unlike
  `variant_patterns.csv`'s digits (ADR-306 item 6).
- Two definitions reading one request at one endpoint send it twice.
- A field whose value equals a candidate's `expected` as a prefix but not as the whole value
  matches, as patterns do (ADR-306 Consequences).
- The server recomputes with the same `contradictions`, `expected_matches` and class rules.
- ADR-397: `identification_request_of` is extracted from `identification_requests`,
  `expected_matches` is `pattern_matches`' comparator made public, `expected_value` shares its
  sign extension, and the hypothetical classes use `contradictions` and `fold_outcomes` unchanged.
