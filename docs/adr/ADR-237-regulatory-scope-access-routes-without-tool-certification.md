# ADR-237: Regulatory Scope: Use the Access Routes Regulations Open, Without Tool Certification

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** design 8.1, 9.2, 16.1, 16.2, 17 (P7 removed); `db/` retention (`retain_until`); generic OBD vehicle-knowledge package (not yet implemented)

## Context

Design 17 P7 left open which vehicle regulations the system supports: whether to follow the US
CARB / EPA generic scan tool requirements and the EU repairer access rules was said to depend on
the target markets. Design 16.2 already covers UNECE R155 / R156, VINs as personal data, rights to
ODX / PDX data and SAE J3138.

A survey of the main markets (October 2026) found that almost every regulation places its
obligations on vehicle makers or on repair and inspection businesses, not on diagnostic software:

- **United States.** CARB and EPA OBD rules require vehicles to report emissions-related
  diagnostic data to a generic scan tool (SAE J1979; for heavy-duty vehicles SAE J1979-2,
  "OBD on UDS", phases in from model year 2027, earlier for some makers). CARB's service
  information rules and state right-to-repair laws require makers to offer reprogramming through
  SAE J2534 pass-thru devices (TMC RP1210 is also allowed for heavy-duty). A tool only carries its
  own obligations when it is offered as a CARB-certified OBD test tool for inspection programs.
- **European Union.** Regulation (EU) 2018/858, Annex X (amended by Delegated Regulation (EU)
  2021/1244), requires makers to give independent operators non-discriminatory access to OBD
  data and repair and maintenance information (RMI), including diagnosis and reprogramming with
  standard pass-thru interfaces. Access to security-related RMI (immobiliser, key programming,
  theft protection) goes through SERMI: a conformity assessment body accredits the business and
  its staff, and the maker's portal checks that accreditation. The Data Act and Euro 7 do not yet
  add requirements on tools.
- **Japan.** OBD inspection (from October 2024, imported vehicles from October 2025) reads
  specified DTCs with an application provided by the National Agency for Automobile and Land
  Transport Technology, run on a scan tool that meets the inspection requirements. Specified
  maintenance (tokutei seibi) certifies the repair business, not its tools.

So the system is directly regulated only if it seeks certification as an inspection tool.
Everything else is a question of whether the system can use the access routes that the
regulations force makers to open: generic OBD, reprogramming through standard pass-thru
interfaces, and security-related functions behind the maker's authentication.

The options were: (A) support those access routes and seek no tool certification; (B) A plus
certification as one market's inspection tool; (C) leave generic OBD out and support only
maker-specific diagnosis from ODX or the proprietary format; (D) keep P7 open until the target
markets are chosen. The maintainer chose A.

## Decision

1. **No inspection-tool certification.** The system is not built or certified as a regulated
   inspection tool (a CARB-certified OBD test tool, a scan tool for Japan's OBD inspection, or
   similar). Pursuing one later is a separate decision that adds to the items below without
   changing them.
2. **Generic OBD is part of the standard function set.** Reading emissions-related generic OBD
   data (SAE J1979 / ISO 15031-5 on ISO 15765-4, SAE J1979-2, and WWH-OBD per ISO 27145) is
   provided as a standard vehicle-knowledge package: L3 data in the IR (8.1, 8.2), distributed and
   signed like any other extension package, running on the shared L2 primitives. The system does
   not depend on a maker's ODX for these reads.
3. **Reprogramming routes stay within the existing design.** The system's own reprogramming jobs
   (5.6, 8.2.5) use VCIs through the J2534 and D-PDU API workers. The system does not present
   itself as a J2534 or D-PDU API library to a maker's own reprogramming application; when such an
   application uses the VCI directly, the exclusive control in 8.8 only detects it. TMC RP1210 is
   out of scope.
4. **Security-related access goes through the OEM authentication provider.** SERMI and similar
   maker schemes are handled by the OEM authentication provider extension point (9.2) under the
   rules of 8.10. Accreditations and certificates belong to the business and its staff and stay
   with the maker's portal or the provider; the system does not hold them. The audit log records
   which operator ran the operation and the authorization reference the provider returns.
5. **Retention periods are operator settings.** Statutory retention periods for maintenance
   records and update records differ by jurisdiction. The framework provides a configurable
   retention period per tenant and record kind and ships no jurisdiction-specific defaults; the
   operator sets them (16.1).
6. **Regulations without tool requirements are watched, not implemented.** Data-access rules
   such as the EU Data Act and Euro 7's on-board monitoring are revisited when they place
   requirements on diagnostic tools.

## Consequences

- Generic OBD needs functional (broadcast) requests answered by several ECUs at L1/L2, and the
  standards that define the generic services and identifiers (SAE J1979, J1979-2, ISO 15031-5,
  ISO 15765-4, ISO 27145), none of which is held yet.
- The responsibility table in 16.1 gains a row: certification, accreditation and statutory
  retention periods are the operator's.
- A business that wants to use the system for statutory inspection cannot do so without the
  separate decision in item 1.
- Japan's current inspection scan tool requirements were not found in public sources during the
  survey; they are needed only if item 1 is revisited.
